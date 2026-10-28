use std::path::PathBuf;

use anyhow::{Context, bail};
use serde_json::{Value, json};

use super::app_server::{AppServerConfig, CodexAppServer};

const RESERVED_KEY: &str = "codex-plus-weixin-reserved-v1";

fn is_reserved(project: &Value) -> bool {
    project["metadata"]["codexPlusWeixin"].as_str() == Some(RESERVED_KEY)
}

pub async fn read_projects(server: &mut CodexAppServer) -> anyhow::Result<Vec<Value>> {
    let mut projects = Vec::new();
    let mut cursor = Value::Null;
    loop {
        let response = server
            .api_request("project/list", json!({"cursor":cursor,"limit":100}))
            .await?;
        projects.extend(
            response["data"]
                .as_array()
                .context("项目列表响应缺少 data")?
                .iter()
                .cloned(),
        );
        let next = response["nextCursor"].clone();
        if next.is_null() {
            break;
        }
        if next == cursor || projects.len() > 10_000 {
            bail!("项目列表分页异常");
        }
        cursor = next;
    }
    Ok(projects)
}

/// 目录读取绝不创建预留项目；名称始终由原生项目 ID 对应的记录提供。
pub async fn list_projects(executable: String, work_dir: PathBuf) -> anyhow::Result<Value> {
    let mut server = CodexAppServer::start(AppServerConfig {
        executable,
        work_dir,
        model: String::new(),
        allow_outbound_attachments: false,
        reasoning_effort: String::new(),
        project_id: String::new(),
        sandbox: "read-only".into(),
    })
    .await?;
    let result = async {
        let projects = read_projects(&mut server).await?;
        let reserved_id = projects
            .iter()
            .find(|project| is_reserved(project))
            .and_then(|project| project["id"].as_str());
        Ok(json!({"reservedProjectId": reserved_id, "projects": projects}))
    }
    .await;
    server.close().await;
    result
}

/// 仅在实际处理聊天时调用。幂等键使中断重试不会创建第二个预留项目。
pub async fn resolve_project(
    server: &mut CodexAppServer,
    selected_id: &str,
    work_dir: &std::path::Path,
) -> anyhow::Result<Value> {
    if !selected_id.is_empty() {
        let result = server
            .api_request("project/read", json!({"projectId":selected_id}))
            .await?;
        let project = result.get("project").context("所选工作项目不可用")?;
        if project["id"].as_str() != Some(selected_id) {
            bail!("工作项目 ID 不匹配");
        }
        return Ok(project.clone());
    }
    if let Some(project) = read_projects(server).await?.into_iter().find(is_reserved) {
        return Ok(project);
    }
    let result = server
        .api_request(
            "project/create",
            json!({
                "idempotencyKey": RESERVED_KEY,
                "name": "微信Bot",
                "roots": [{"path":work_dir.to_string_lossy()}],
                "metadata": {"codexPlusWeixin":RESERVED_KEY}
            }),
        )
        .await?;
    let project = result
        .get("project")
        .cloned()
        .context("创建微信工作项目未返回项目")?;
    if project["id"].as_str().is_none_or(str::is_empty) || !is_reserved(&project) {
        bail!("创建微信工作项目返回了无效的原生项目身份");
    }
    Ok(project)
}

/// 切换项目时移动整个原线程（含历史），不创建新线程，也不只修改本回合。
pub async fn assign_thread(
    server: &mut CodexAppServer,
    thread_id: &str,
    project_id: &str,
) -> anyhow::Result<()> {
    let result = server
        .api_request(
            "thread/metadata/update",
            json!({"threadId":thread_id,"projectId":project_id}),
        )
        .await?;
    if result["thread"]["id"].as_str() != Some(thread_id)
        || result["thread"]["projectId"].as_str() != Some(project_id)
    {
        bail!("原生线程项目归属未确认，原会话映射已保留，尚未发送回合");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_reserved_project_is_recognized_by_identity_metadata() {
        assert!(is_reserved(
            &json!({"id":"p1","name":"Renamed","metadata":{"codexPlusWeixin":RESERVED_KEY}})
        ));
        assert!(!is_reserved(
            &json!({"id":"p2","name":"微信Bot","metadata":{}})
        ));
    }
}
