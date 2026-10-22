/**
 * ZCode 语音输入 —— 页面侧脚本
 *
 * 由 Codex++ 管理端通过 CDP 注入（Page.addScriptToEvaluateOnNewDocument + 当前页补一次
 * Runtime.evaluate，所以必须幂等）。与 assets/claude-localization/page-dictation.js 不同：
 * ZCode 的渲染层没有麦克风 entitlement，录音与 ASR 全部在 Codex++ 管理端完成，页面只做三件事：
 *
 *   1. 在当前 composer 工具条里放一个麦克风按钮（多分屏每个 pane 各一份）；
 *   2. 点击时 POST 本地回环服务 /zcode-voice/{start,stop,cancel}；
 *   3. 拿到识别文本后，写进 ZCode 的 Lexical 输入框（优先用生产包自带的 E2E 桥做 JSON 追加，
 *      保留已有的 @mention 等富文本节点；桥不可用时退回 setText / execCommand）。
 *
 * 通道约定（管理端 voice_server.rs 实现）：
 *   window.__zcodePlusVoiceConfig = { endpoint: "http://127.0.0.1:<port>", token: "<token>" }
 *   POST {endpoint}/zcode-voice/start?token=...   -> {"status":"ok"}
 *   POST {endpoint}/zcode-voice/stop?token=...    -> {"status":"ok","text":"..."}
 *   POST {endpoint}/zcode-voice/cancel?token=...  -> {"status":"ok"}
 *   请求不带 body、不带自定义 header（跨源预检只允许简单请求），credentials: "omit"。
 *   页面是 file:// 无 CSP，`fetch("http://127.0.0.1:...")` 可用，响应必须带
 *   Access-Control-Allow-Origin（由服务端保证）。
 *
 * 生命周期：宿主是 React + Lexical，composer、工具条、输入框会被反复重建；这里不缓存 DOM，
 * 靠 MutationObserver + rAF 重挂。注入多次只有第一份生效，window.__zcodePlusVoiceDispose()
 * 负责完全清理（管理端关闭语音输入时会调用）。
 *
 * 自喂防护：脚本自己创建的节点都带 data-zcode-plus-voice 属性；MutationObserver 回调里
 * 忽略「增删节点全是自己节点」的记录，避免「挂按钮 -> observer -> 再挂按钮」的循环。
 *
 * 已知风险：composer 选择器、工具条属性和 __zcodeLexicalInputE2E 桥都是 ZCode 3.16.2 的
 * 内部实现，升级后可能失效。失效时按钮挂不上或退化为浮动按钮/execCommand 路径，不会导致
 * 宿主崩溃；选择器集中在文件顶部常量区，便于按版本调整。
 */

(function () {
  'use strict';

  // ---------- 常量 ----------

  /** 所有注入节点都带这个属性，用于去重、自身节点判定与清理。 */
  var MARK_ATTR = 'data-zcode-plus-voice';
  var BUTTON_SELECTOR = '[' + MARK_ATTR + '="true"]';
  var OWN_SELECTOR = '[' + MARK_ATTR + ']';
  var STYLE_ID = 'zcode-plus-voice-style';

  /** ZCode 3.16.2 生产包内部选择器，集中在这里方便版本升级时调整。 */
  var INPUT_SELECTOR = '[data-testid="v4-composer-input"]';
  var COMPOSER_SELECTOR = '[data-testid="v4-composer"]';
  var TOOLBAR_SELECTOR = '[data-composer-trailing-actions]';
  var SEND_SELECTOR = '[data-testid="v4-composer-send"]';
  var PANE_SELECTOR = '[data-pane-id]';

  var BASE_CLASS = 'zcode-plus-voice-button';
  var FLOATING_CLASS = 'zcode-plus-voice-floating';
  var TOAST_CLASS = 'zcode-plus-voice-toast';

  var PATH_START = '/zcode-voice/start';
  var PATH_STOP = '/zcode-voice/stop';
  var PATH_CANCEL = '/zcode-voice/cancel';

  /**
   * 连接本机服务的超时（start / cancel），单位毫秒。
   * 首次点麦克风会触发 macOS 的 TCC 授权弹窗，用户点「允许」之前本机服务可能
   * 一直阻塞在打开设备上，所以这里给到 30 秒；授权过一次后就是毫秒级。
   */
  var CONNECT_TIMEOUT_MS = 30000;
  /** stop 要等 ASR 出结果，可能很慢，给到 120 秒。 */
  var STOP_TIMEOUT_MS = 120000;
  /** 录到 5 分钟自动停，避免用户忘了点停止一直占着麦克风。 */
  var MAX_RECORD_MS = 5 * 60 * 1000;
  /** toast 停留时间（2~4 秒）。 */
  var TOAST_MS = 3200;

  var MSG_NOT_READY = '语音服务未就绪，请在 Codex++ 中开启 ZCode 语音输入';
  var MSG_TIMEOUT = '等待语音服务超时：首次使用请先在系统弹窗里允许麦克风，再重试';
  var MSG_NETWORK = '无法连接语音服务，请确认 Codex++ 正在运行';
  var MSG_FETCH_MISSING = '当前环境不支持网络请求';

  var LABELS = {
    idle: '语音输入',
    recording: '停止录音',
    stopping: '正在识别…',
  };

  // ---------- 状态 ----------

  var state = {
    /** idle | recording | stopping */
    phase: 'idle',
    /** start / stop 请求进行中，防止并发点击。 */
    pending: false,
    /** 本次录音发起时所在的 pane，停止后优先把文本写回这个 pane。 */
    paneId: null,
    /** 5 分钟自动停止定时器。 */
    maxTimer: 0,
    /** 启动阶段的补扫定时器（0 / 1.5s / 5s）。 */
    bootTimers: [],
    /** 2.5 秒低频兜底扫描。 */
    interval: 0,
    /** toast 节点与定时器。 */
    toast: null,
    toastTimer: 0,
    observer: null,
    scheduled: false,
    disposed: false,
  };

  /** 记录本次安装的 window.insertVoiceText，dispose 时只删自己那份。 */
  var installedInsertFn = null;

  // ---------- 通用小工具 ----------

  function noop() {}

  function hasAttributeSafe(node, name) {
    return !!(node && typeof node.hasAttribute === 'function' && node.hasAttribute(name));
  }

  function getAttributeSafe(node, name) {
    if (!node || typeof node.getAttribute !== 'function') return null;
    try {
      return node.getAttribute(name);
    } catch (e) {
      return null;
    }
  }

  function removeNode(node) {
    try {
      if (node && node.parentNode) node.parentNode.removeChild(node);
    } catch (e) {}
  }

  function indexOfNode(list, node) {
    if (!list) return -1;
    for (var i = 0; i < list.length; i++) {
      if (list[i] === node) return i;
    }
    return -1;
  }

  function isArray(value) {
    return Object.prototype.toString.call(value) === '[object Array]';
  }

  /** 节点自身或任一祖先是否属于本脚本注入的节点。 */
  function isOwnNode(node) {
    var current = node;
    while (current) {
      if (current.nodeType === 1 && hasAttributeSafe(current, MARK_ATTR)) return true;
      current = current.parentNode;
    }
    return false;
  }

  /** node 自身或祖先链上的 data-pane-id。 */
  function getPaneId(node) {
    var current = node;
    while (current && current.nodeType === 1) {
      var paneId = getAttributeSafe(current, 'data-pane-id');
      if (paneId) return paneId;
      current = current.parentNode;
    }
    return null;
  }

  function closestComposer(node) {
    var current = node;
    while (current && current.nodeType === 1) {
      if (getAttributeSafe(current, 'data-testid') === 'v4-composer') return current;
      current = current.parentNode;
    }
    return null;
  }

  /** 是否位于 inert / aria-hidden 的容器里（隐藏分屏、后台面板）。 */
  function isHiddenNode(node) {
    var current = node;
    while (current && current.nodeType === 1) {
      if (hasAttributeSafe(current, 'inert')) return true;
      if (getAttributeSafe(current, 'aria-hidden') === 'true') return true;
      current = current.parentNode;
    }
    return false;
  }

  /** 有布局盒才算可见；拿不到布局信息时宽松放行（便于测试与降级环境）。 */
  function isVisibleNode(node) {
    if (!node || typeof node.getBoundingClientRect !== 'function') return true;
    try {
      var rect = node.getBoundingClientRect();
      if (!rect) return true;
      return rect.width > 0 || rect.height > 0;
    } catch (e) {
      return true;
    }
  }

  /** 输入框是否可写：连接在文档里、contenteditable 未关闭、不是隐藏 pane。 */
  function isUsableInput(input) {
    if (!input || input.nodeType !== 1) return false;
    if (input.isConnected === false) return false;
    if (getAttributeSafe(input, 'contenteditable') === 'false') return false;
    if (getAttributeSafe(input, 'aria-readonly') === 'true') return false;
    if (isHiddenNode(input)) return false;
    return true;
  }

  /** 在一组输入框里挑第一个可写的。 */
  function pickUsableInput(inputs) {
    if (!inputs) return null;
    for (var i = 0; i < inputs.length; i++) {
      if (isUsableInput(inputs[i])) return inputs[i];
    }
    return null;
  }

  // ---------- 样式 ----------

  function installStyle() {
    if (!document.documentElement) return;
    if (typeof document.getElementById === 'function' && document.getElementById(STYLE_ID)) return;
    var style = document.createElement('style');
    style.id = STYLE_ID;
    style.setAttribute(MARK_ATTR, 'style');
    style.textContent =
      '.' + BASE_CLASS + '{display:inline-flex;align-items:center;justify-content:center;' +
      'align-self:center;width:30px;height:30px;padding:0;margin:0 2px;border:0;border-radius:8px;' +
      'background:transparent;color:var(--color-foreground-subtle,#6b7280);opacity:.85;cursor:pointer;' +
      'flex:0 0 auto;transition:opacity .15s,background .15s,color .15s}' +
      '.' + BASE_CLASS + ':hover{opacity:1;background:var(--color-surface-hover,rgba(127,127,127,.14));' +
      'color:var(--color-foreground,#111)}' +
      '.' + BASE_CLASS + '[data-phase="recording"]{opacity:1;color:#e5484d;' +
      'background:rgba(229,72,77,.16);animation:zcode-plus-voice-pulse 1.4s ease-in-out infinite}' +
      '.' + BASE_CLASS + '[data-phase="stopping"]{opacity:.85;cursor:progress}' +
      '.' + BASE_CLASS + '[data-phase="stopping"] svg{animation:zcode-plus-voice-spin 1s linear infinite}' +
      '.' + BASE_CLASS + '[disabled]{opacity:.45;cursor:not-allowed}' +
      '.' + FLOATING_CLASS + '{position:absolute;right:10px;bottom:10px;z-index:5;' +
      'background:var(--color-background,#fff);box-shadow:0 2px 10px rgba(0,0,0,.18)}' +
      '.' + TOAST_CLASS + '{position:fixed;left:50%;bottom:96px;transform:translateX(-50%);' +
      'z-index:2147483000;max-width:min(420px,80vw);padding:8px 14px;border-radius:10px;' +
      'font-size:13px;line-height:18px;background:rgba(28,28,30,.94);color:#fff;' +
      'box-shadow:0 6px 24px rgba(0,0,0,.28);pointer-events:none}' +
      '@keyframes zcode-plus-voice-pulse{0%,100%{opacity:1}50%{opacity:.55}}' +
      '@keyframes zcode-plus-voice-spin{from{transform:rotate(0)}to{transform:rotate(360deg)}}';
    var host = document.head || document.documentElement;
    if (host) host.appendChild(style);
  }

  // ---------- 图标 ----------

  function svgElement(tagName) {
    if (typeof document.createElementNS === 'function') {
      try {
        return document.createElementNS('http://www.w3.org/2000/svg', tagName);
      } catch (e) {}
    }
    return document.createElement(tagName);
  }

  function makeSvg() {
    var svg = svgElement('svg');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('width', '16');
    svg.setAttribute('height', '16');
    svg.setAttribute('aria-hidden', 'true');
    svg.setAttribute('focusable', 'false');
    return svg;
  }

  function micIcon() {
    var svg = makeSvg();
    svg.setAttribute('fill', 'none');
    svg.setAttribute('stroke', 'currentColor');
    svg.setAttribute('stroke-width', '1.8');
    svg.setAttribute('stroke-linecap', 'round');
    svg.setAttribute('stroke-linejoin', 'round');
    var body = svgElement('path');
    body.setAttribute('d', 'M12 3.5a3 3 0 0 0-3 3v5a3 3 0 0 0 6 0v-5a3 3 0 0 0-3-3Z');
    var arc = svgElement('path');
    arc.setAttribute('d', 'M5.5 11.5a6.5 6.5 0 0 0 13 0M12 18v2.5');
    svg.appendChild(body);
    svg.appendChild(arc);
    return svg;
  }

  function stopIcon() {
    var svg = makeSvg();
    svg.setAttribute('fill', 'currentColor');
    svg.setAttribute('stroke', 'none');
    var rect = svgElement('rect');
    rect.setAttribute('x', '7');
    rect.setAttribute('y', '7');
    rect.setAttribute('width', '10');
    rect.setAttribute('height', '10');
    rect.setAttribute('rx', '2');
    svg.appendChild(rect);
    return svg;
  }

  function spinnerIcon() {
    var svg = makeSvg();
    svg.setAttribute('fill', 'none');
    svg.setAttribute('stroke', 'currentColor');
    svg.setAttribute('stroke-width', '2');
    svg.setAttribute('stroke-linecap', 'round');
    var path = svgElement('path');
    path.setAttribute('d', 'M12 3.5a8.5 8.5 0 1 0 8.5 8.5');
    svg.appendChild(path);
    return svg;
  }

  // ---------- 提示 ----------

  function notify(message) {
    try {
      if (!document.body && !document.documentElement) return;
      if (state.toast) removeNode(state.toast);
      var node = document.createElement('div');
      node.className = TOAST_CLASS;
      node.setAttribute(MARK_ATTR, 'toast');
      node.textContent = String(message == null ? '' : message);
      (document.body || document.documentElement).appendChild(node);
      state.toast = node;
      clearTimeout(state.toastTimer);
      state.toastTimer = setTimeout(function () {
        removeNode(node);
        if (state.toast === node) state.toast = null;
      }, TOAST_MS);
    } catch (e) {}
  }

  // ---------- 本机服务调用 ----------

  /** 读取注入前缀给的连接信息；配置缺失时返回 null，按钮仍可用（点击时提示未就绪）。 */
  function getConfig() {
    var raw = window.__zcodePlusVoiceConfig;
    if (!raw) return null;
    if (typeof raw === 'string') {
      try {
        raw = JSON.parse(raw);
      } catch (e) {
        return null;
      }
    }
    if (!raw || typeof raw !== 'object') return null;
    var endpoint = raw.endpoint;
    var token = raw.token;
    if (typeof endpoint !== 'string' || !endpoint) return null;
    if (typeof token !== 'string' || !token) return null;
    // 去掉尾部斜杠，拼接路径时统一只留一个分隔符。
    endpoint = endpoint.replace(/\/+$/, '');
    return { endpoint: endpoint, token: token };
  }

  function friendlyError(error) {
    if (!error) return '语音操作失败，请重试';
    var message = typeof error.message === 'string' ? error.message : '';
    if (!message) return '语音操作失败，请重试';
    // fetch 在断网 / 服务未启动时给的是英文 TypeError 文案，换成用户能看懂的中文。
    if (error.name === 'AbortError') return MSG_TIMEOUT;
    if (/(failed to fetch|networkerror|network error|load failed)/i.test(message)) return MSG_NETWORK;
    return message;
  }

  /**
   * POST 本机语音服务。
   *
   * 约束：不写 body、不设自定义 header、credentials "omit" —— 让请求保持 CORS 简单请求，
   * 不触发 OPTIONS 预检。token 走 query 参数（服务端按 token 鉴权）。
   */
  function postCommand(path, timeoutMs) {
    return new Promise(function (resolve, reject) {
      var config = getConfig();
      if (!config) {
        reject(new Error(MSG_NOT_READY));
        return;
      }
      if (typeof window.fetch !== 'function') {
        reject(new Error(MSG_FETCH_MISSING));
        return;
      }
      var url = config.endpoint + path + '?token=' + encodeURIComponent(config.token);
      var settled = false;
      var timer = 0;
      var controller = null;

      function finish(callback, value) {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        callback(value);
      }

      timer = setTimeout(function () {
        try {
          if (controller && typeof controller.abort === 'function') controller.abort();
        } catch (e) {}
        finish(reject, new Error(MSG_TIMEOUT));
      }, timeoutMs);

      if (typeof AbortController === 'function') {
        try {
          controller = new AbortController();
        } catch (e) {
          controller = null;
        }
      }

      var options = { method: 'POST', credentials: 'omit' };
      if (controller) options.signal = controller.signal;

      var request;
      try {
        // 用 window.fetch(...) 的形式调用，保留 Window 作为 this
        // （Chrome 里解构后的 fetch 会 Illegal invocation）。
        request = window.fetch(url, options);
      } catch (e) {
        finish(reject, new Error(MSG_NETWORK));
        return;
      }

      Promise.resolve(request)
        .then(function (response) {
          if (!response || !response.ok) {
            var status = response && response.status;
            finish(reject, new Error('语音服务返回异常' + (status ? '（HTTP ' + status + '）' : '')));
            return null;
          }
          return response.json();
        })
        .then(function (data) {
          if (settled) return;
          if (!data || data.status !== 'ok') {
            finish(reject, new Error((data && data.message) || '语音服务操作失败'));
            return;
          }
          finish(resolve, data);
        })
        .catch(function (error) {
          if (settled) return;
          finish(reject, new Error(friendlyError(error)));
        });
    });
  }

  // ---------- 按钮与挂载 ----------

  function makeButton(isFloating) {
    var button = document.createElement('button');
    button.type = 'button';
    button.className = isFloating ? BASE_CLASS + ' ' + FLOATING_CLASS : BASE_CLASS;
    button.setAttribute(MARK_ATTR, 'true');
    button.setAttribute('data-phase', 'idle');
    button.setAttribute('aria-label', LABELS.idle);
    button.setAttribute('title', LABELS.idle);
    // 工具条按钮默认会抢焦点、触发提交快捷键，这里全部拦住。
    button.addEventListener('mousedown', function (event) {
      if (event && typeof event.preventDefault === 'function') event.preventDefault();
    });
    button.addEventListener('keydown', function (event) {
      if (event && typeof event.stopPropagation === 'function') event.stopPropagation();
    });
    button.addEventListener('click', function (event) {
      handleButtonClick(event, button);
    });
    return button;
  }

  function renderButton(button) {
    if (!button) return;
    var phase = state.phase;
    var previous = button.getAttribute('data-phase');
    button.setAttribute('data-phase', phase);
    button.setAttribute('aria-label', LABELS[phase]);
    button.setAttribute('title', LABELS[phase]);
    button.setAttribute('aria-pressed', phase === 'recording' ? 'true' : 'false');
    button.disabled = phase === 'stopping';
    // 状态没变就不重建图标：宿主每 2.5 秒扫一次，没必要反复造 SVG。
    if (previous === phase && button.firstChild) return;
    var icon;
    if (phase === 'stopping') icon = spinnerIcon();
    else if (phase === 'recording') icon = stopIcon();
    else icon = micIcon();
    if (typeof button.replaceChildren === 'function') {
      button.replaceChildren(icon);
    } else {
      while (button.firstChild) button.removeChild(button.firstChild);
      button.appendChild(icon);
    }
  }

  function renderButtons() {
    var buttons;
    try {
      buttons = document.querySelectorAll(BUTTON_SELECTOR);
    } catch (e) {
      return;
    }
    for (var i = 0; i < buttons.length; i++) renderButton(buttons[i]);
  }

  /** 同一 composer 里的注入按钮只留一个；多余的是宿主重建后遗留的孤儿。 */
  function keepSingleButton(composer) {
    var buttons = composer.querySelectorAll ? composer.querySelectorAll(BUTTON_SELECTOR) : [];
    for (var i = buttons.length - 1; i >= 1; i--) removeNode(buttons[i]);
    return buttons.length ? buttons[0] : null;
  }

  /** 浮动兜底：composer 本身不一定是定位元素，保证绝对定位有参照系。 */
  function ensurePositioned(composer) {
    try {
      var position = composer.style && composer.style.position;
      if (!position && typeof window.getComputedStyle === 'function') {
        var computed = window.getComputedStyle(composer);
        position = computed && computed.position;
      }
      if (!position || position === 'static') composer.style.position = 'relative';
    } catch (e) {}
  }

  function mountInComposer(scope) {
    var composer = scope && scope.composer;
    if (!composer || composer.nodeType !== 1) return;

    var toolbar = null;
    try {
      toolbar = composer.querySelector ? composer.querySelector(TOOLBAR_SELECTOR) : null;
    } catch (e) {
      toolbar = null;
    }
    var button = keepSingleButton(composer);

    if (toolbar) {
      if (!button) {
        button = makeButton(false);
        var send = null;
        try {
          send = toolbar.querySelector ? toolbar.querySelector(SEND_SELECTOR) : null;
        } catch (e) {
          send = null;
        }
        if (send && send.parentNode === toolbar) toolbar.insertBefore(button, send);
        else toolbar.appendChild(button);
      } else {
        // 按钮曾在浮动兜底位置，现在工具条回来了：挪进工具条并去掉浮动样式。
        if (button.parentNode !== toolbar) toolbar.appendChild(button);
        button.className = BASE_CLASS;
      }
    } else {
      if (!button) {
        button = makeButton(true);
        ensurePositioned(composer);
        composer.appendChild(button);
      } else if (button.parentNode !== composer) {
        composer.appendChild(button);
      }
    }
    renderButton(button);
  }

  /** 从当前 DOM 重新收集挂载点，不保留任何旧引用。 */
  function collectScopes() {
    var scopes = [];
    var seen = [];

    function addScope(composer, paneId) {
      if (!composer || composer.nodeType !== 1) return;
      if (indexOfNode(seen, composer) >= 0) return;
      seen.push(composer);
      scopes.push({ composer: composer, paneId: paneId || null });
    }

    var panes = document.querySelectorAll(PANE_SELECTOR);
    for (var i = 0; i < panes.length; i++) {
      var pane = panes[i];
      var paneId = getAttributeSafe(pane, 'data-pane-id');
      var composer = null;
      try {
        if (getAttributeSafe(pane, 'data-testid') === 'v4-composer') composer = pane;
        else composer = pane.querySelector ? pane.querySelector(COMPOSER_SELECTOR) : null;
      } catch (e) {
        composer = null;
      }
      if (!composer) {
        // 退化：pane 里没有 v4-composer 外壳时，用输入框所在容器当作用域。
        var paneInput = null;
        try {
          paneInput = pane.querySelector ? pane.querySelector(INPUT_SELECTOR) : null;
        } catch (e2) {
          paneInput = null;
        }
        if (paneInput) composer = closestComposer(paneInput) || pane;
      }
      addScope(composer, paneId);
    }

    var composers = document.querySelectorAll(COMPOSER_SELECTOR);
    for (var j = 0; j < composers.length; j++) {
      addScope(composers[j], getPaneId(composers[j]));
    }

    // 最后一档：没有 pane、也没有 v4-composer 外壳时，按输入框找它的父容器。
    if (!scopes.length) {
      var inputs = document.querySelectorAll(INPUT_SELECTOR);
      for (var k = 0; k < inputs.length; k++) {
        var input = inputs[k];
        var owner = closestComposer(input) || input.parentNode;
        if (owner && owner.nodeType === 1) addScope(owner, getPaneId(input));
      }
    }
    return scopes;
  }

  /** 清掉宿主挪到别处、已经不属于任何 composer 的注入按钮。 */
  function cleanupOrphans() {
    var buttons = document.querySelectorAll(BUTTON_SELECTOR);
    for (var i = buttons.length - 1; i >= 0; i--) {
      if (!closestComposer(buttons[i])) removeNode(buttons[i]);
    }
  }

  function scan() {
    if (state.disposed || !document.documentElement) return;
    try {
      installStyle();
    } catch (e) {}
    try {
      var scopes = collectScopes();
      for (var i = 0; i < scopes.length; i++) mountInComposer(scopes[i]);
      cleanupOrphans();
      renderButtons();
    } catch (error) {}
  }

  var schedule = function () {
    if (state.disposed || state.scheduled) return;
    state.scheduled = true;
    var run = function () {
      state.scheduled = false;
      scan();
    };
    if (typeof requestAnimationFrame === 'function') requestAnimationFrame(run);
    else setTimeout(run, 16);
  };

  // ---------- MutationObserver ----------

  /** 增删的都是自身节点 => 是我们自己挂 UI 引起的，忽略掉避免自喂扫描。 */
  function mutationNeedsScan(records) {
    if (!records) return false;
    for (var i = 0; i < records.length; i++) {
      var record = records[i];
      if (!record) continue;
      var added = record.addedNodes || [];
      var removed = record.removedNodes || [];
      var anyNode = false;
      var allOwn = true;
      for (var a = 0; a < added.length; a++) {
        anyNode = true;
        if (!isOwnNode(added[a])) allOwn = false;
      }
      for (var r = 0; r < removed.length; r++) {
        anyNode = true;
        if (!isOwnNode(removed[r])) allOwn = false;
      }
      if (!anyNode) {
        // 属性变化：宿主节点上的属性变化值得重扫，自身节点忽略。
        if (!isOwnNode(record.target)) return true;
        continue;
      }
      // 纯自身节点的「新增」忽略；纯自身节点的「移除」如果是宿主容器发起的，
      // 需要重扫把按钮补回去（例如宿主单独摘掉了我们的按钮）。
      if (allOwn && (added.length > 0 || isOwnNode(record.target))) continue;
      return true;
    }
    return false;
  }

  function installObserver() {
    if (typeof MutationObserver !== 'function') return;
    try {
      state.observer = new MutationObserver(function (records) {
        if (state.disposed) return;
        if (mutationNeedsScan(records)) schedule();
      });
      // 脚本可能通过 addScriptToEvaluateOnNewDocument 在 documentElement 出现之前执行，
      // 所以优先观察 document（subtree 覆盖整棵树），拿不到再退回 documentElement。
      var target = document.documentElement || document;
      state.observer.observe(target, { childList: true, subtree: true });
    } catch (e) {
      state.observer = null;
    }
  }

  // ---------- 文本插入 ----------

  function makeTextNode(text) {
    return {
      detail: 0,
      format: 0,
      mode: 'normal',
      style: '',
      text: text,
      type: 'text',
      version: 1,
    };
  }

  function makeParagraph(children) {
    return {
      children: children,
      direction: null,
      format: '',
      indent: 0,
      type: 'paragraph',
      version: 1,
      textFormat: 0,
      textStyle: '',
    };
  }

  /** 取最后一段的末尾内容；非文本节点（mention 等）视作有内容。 */
  function tailContent(children) {
    if (!children || !children.length) return '';
    var last = children[children.length - 1];
    if (!last) return '';
    if (last.type === 'text' && typeof last.text === 'string') return last.text;
    return 'x';
  }

  /** 把识别文本追加到 Lexical JSON：优先并入最后一段，否则新起一段。 */
  function appendVoiceNode(root, text) {
    if (!isArray(root.children)) root.children = [];
    var children = root.children;
    var last = children.length ? children[children.length - 1] : null;
    if (last && last.type === 'paragraph' && typeof last === 'object') {
      if (!isArray(last.children)) last.children = [];
      var voiceText = text;
      var tail = tailContent(last.children);
      // 有已有内容且结尾不是空白时补一个空格，避免与前面的文字粘在一起；
      // 以空白或中文/英文标点开头时不补（识别结果通常自带标点）。
      if (tail && !/\s$/.test(tail) && !/^[\s，。！？、,.;；:：）)】》>]/.test(text)) {
        voiceText = ' ' + text;
      }
      last.children.push(makeTextNode(voiceText));
      return;
    }
    children.push(makeParagraph([makeTextNode(text)]));
  }

  function editorStateToJson(bridge) {
    var editorState = bridge.getEditorState();
    if (!editorState) return null;
    if (typeof editorState === 'string') return JSON.parse(editorState);
    if (typeof editorState.toJSON === 'function') return editorState.toJSON();
    // 有的版本 EditorState 不走 toJSON，只能做一次深拷贝。
    return JSON.parse(JSON.stringify(editorState));
  }

  function containsText(readBack, text) {
    if (typeof readBack !== 'string') return true; // 无法回读时不判失败
    var haystack = readBack.replace(/\s+/g, '');
    var needle = String(text).replace(/\s+/g, '');
    if (!needle) return true;
    return haystack.indexOf(needle) >= 0;
  }

  /**
   * 写入尝试的三种结果。
   *
   * WRITE_UNVERIFIED 单独区分出来，是为了避免「JSON 已经写进去了，只是回读格式对不上，
   * 然后 setText 再写一遍」导致文字重复。已经落盘但没法确认时不再叠加下一种写法。
   */
  var WRITE_SKIPPED = 0;
  var WRITE_OK = 1;
  var WRITE_UNVERIFIED = 2;

  /** 方案 a：E2E 桥 JSON 追加（保留已有 @mention / 富文本）。 */
  function tryBridgeJson(bridge, text) {
    if (typeof bridge.getEditorState !== 'function' || typeof bridge.setEditorStateJson !== 'function') {
      return WRITE_SKIPPED;
    }
    var json;
    try {
      json = editorStateToJson(bridge);
      if (!json || typeof json !== 'object') return WRITE_SKIPPED;
      json = JSON.parse(JSON.stringify(json));
    } catch (e) {
      return WRITE_SKIPPED;
    }
    var root = json.root && typeof json.root === 'object' ? json.root : json;
    if (!root || typeof root !== 'object') return WRITE_SKIPPED;
    appendVoiceNode(root, text);
    try {
      bridge.setEditorStateJson(JSON.stringify(json));
      if (typeof bridge.focus === 'function') bridge.focus();
    } catch (e) {
      return WRITE_SKIPPED;
    }
    if (typeof bridge.getText !== 'function') return WRITE_OK;
    var readBack;
    try {
      readBack = bridge.getText();
    } catch (e) {
      return WRITE_UNVERIFIED;
    }
    return containsText(readBack, text) ? WRITE_OK : WRITE_UNVERIFIED;
  }

  /** 方案 b：桥只有 setText 时，整段重写（会丢富文本，故排在 JSON 追加之后）。 */
  function tryBridgeSetText(bridge, text) {
    if (typeof bridge.setText !== 'function') return WRITE_SKIPPED;
    var current = '';
    if (typeof bridge.getText === 'function') {
      try {
        current = bridge.getText();
      } catch (e) {
        // 拿不到现有内容就无法安全追加（直接 setText 会把已有草稿清掉），放弃。
        return WRITE_SKIPPED;
      }
      if (typeof current !== 'string') current = '';
    }
    try {
      bridge.setText(current + text);
      if (typeof bridge.focus === 'function') bridge.focus();
    } catch (e) {
      return WRITE_SKIPPED;
    }
    if (typeof bridge.getText !== 'function') return WRITE_OK;
    var readBack;
    try {
      readBack = bridge.getText();
    } catch (e) {
      return WRITE_UNVERIFIED;
    }
    return containsText(readBack, text) ? WRITE_OK : WRITE_UNVERIFIED;
  }

  /** 方案 c：最后兜底，聚焦后 execCommand('insertText')。 */
  function tryExecCommand(input, text) {
    try {
      if (typeof input.focus === 'function') input.focus();
      if (typeof document.execCommand !== 'function') return false;
      return document.execCommand('insertText', false, text) === true;
    } catch (e) {
      return false;
    }
  }

  function inputInScope(scope) {
    if (!scope || typeof scope.querySelectorAll !== 'function') return null;
    var inputs;
    try {
      inputs = scope.querySelectorAll(INPUT_SELECTOR);
    } catch (e) {
      return null;
    }
    return pickUsableInput(inputs);
  }

  function findPaneById(paneId) {
    if (paneId === null || paneId === undefined) return null;
    var panes;
    try {
      panes = document.querySelectorAll(PANE_SELECTOR);
    } catch (e) {
      return null;
    }
    for (var i = 0; i < panes.length; i++) {
      if (String(getAttributeSafe(panes[i], 'data-pane-id')) === String(paneId)) return panes[i];
    }
    return null;
  }

  function findFocusedPane() {
    var panes;
    try {
      panes = document.querySelectorAll(PANE_SELECTOR);
    } catch (e) {
      return null;
    }
    for (var i = 0; i < panes.length; i++) {
      if (getAttributeSafe(panes[i], 'data-focused') === 'true') return panes[i];
    }
    return null;
  }

  /**
   * 定位写入目标输入框：
   * 1) 本次录音发起时所在的 pane（用户从哪个 pane 开始说话就写回哪个）；
   * 2) 当前 focused pane；
   * 3) document.activeElement（如果就是 composer 输入框）；
   * 4) 第一个可见且可写的 composer 输入框。
   */
  function findTargetInput(preferredPaneId) {
    var preferredPane = findPaneById(preferredPaneId);
    if (preferredPane) {
      var preferredInput = inputInScope(preferredPane);
      if (preferredInput) return preferredInput;
    }
    var focusedPane = findFocusedPane();
    if (focusedPane) {
      var focusedInput = inputInScope(focusedPane);
      if (focusedInput) return focusedInput;
    }
    var active = document.activeElement;
    if (isUsableInput(active) && getAttributeSafe(active, 'data-testid') === 'v4-composer-input') return active;
    var inputs;
    try {
      inputs = document.querySelectorAll(INPUT_SELECTOR);
    } catch (e) {
      return null;
    }
    for (var i = 0; i < inputs.length; i++) {
      if (!isUsableInput(inputs[i])) continue;
      if (!isVisibleNode(inputs[i])) continue;
      return inputs[i];
    }
    return null;
  }

  function getBridge(input) {
    if (!input) return null;
    var bridge = input.__zcodeLexicalInputE2E;
    if (!bridge || (typeof bridge !== 'object' && typeof bridge !== 'function')) return null;
    return bridge;
  }

  /**
   * 把识别文本写进 composer，返回是否成功（挂在 window.insertVoiceText 便于测试）。
   *
   * 只写输入框，不发送消息；不缓存输入框引用，每次调用都重新定位。
   */
  function insertVoiceText(text, preferredPaneId) {
    if (text === null || text === undefined) return false;
    var value = typeof text === 'string' ? text : String(text);
    if (!value) return false;
    var input = findTargetInput(preferredPaneId);
    if (!input) return false;
    var bridge = getBridge(input);
    if (bridge) {
      var result = tryBridgeJson(bridge, value);
      if (result === WRITE_OK) return true;
      if (result === WRITE_UNVERIFIED) return false;
      result = tryBridgeSetText(bridge, value);
      if (result === WRITE_OK) return true;
      if (result === WRITE_UNVERIFIED) return false;
    }
    return tryExecCommand(input, value);
  }

  // ---------- 状态机 ----------

  function settleIdle() {
    state.phase = 'idle';
    state.pending = false;
    state.paneId = null;
    clearTimeout(state.maxTimer);
    state.maxTimer = 0;
    renderButtons();
  }

  function startRecording(paneId) {
    if (state.disposed || state.pending || state.phase !== 'idle') return;
    state.pending = true;
    postCommand(PATH_START, CONNECT_TIMEOUT_MS)
      .then(function () {
        if (state.disposed) return;
        state.pending = false;
        state.phase = 'recording';
        state.paneId = paneId || null;
        clearTimeout(state.maxTimer);
        state.maxTimer = setTimeout(function () {
          // 最长录音时间到了自动停止，仍然把识别结果写回输入框。
          if (state.phase === 'recording') stopAndInsert(state.paneId);
        }, MAX_RECORD_MS);
        renderButtons();
      })
      .catch(function (error) {
        if (state.disposed) return;
        settleIdle();
        notify(friendlyError(error));
      });
  }

  function stopAndInsert(paneId) {
    if (state.disposed || state.pending || state.phase !== 'recording') return;
    state.pending = true;
    state.phase = 'stopping';
    clearTimeout(state.maxTimer);
    state.maxTimer = 0;
    var targetPaneId = paneId || state.paneId || null;
    renderButtons();
    postCommand(PATH_STOP, STOP_TIMEOUT_MS)
      .then(function (data) {
        if (state.disposed) return;
        var text = data && typeof data.text === 'string' ? data.text.replace(/^\s+|\s+$/g, '') : '';
        var inserted = false;
        if (text) {
          try {
            inserted = insertVoiceText(text, targetPaneId);
          } catch (e) {
            inserted = false;
          }
        }
        settleIdle();
        if (!text) notify('没有识别到文字，请再试一次');
        else if (!inserted) notify('识别完成，但写入输入框失败');
      })
      .catch(function (error) {
        if (state.disposed) return;
        settleIdle();
        notify(friendlyError(error));
      });
  }

  function cancelRecording() {
    if (state.disposed || state.phase !== 'recording') return;
    clearTimeout(state.maxTimer);
    state.maxTimer = 0;
    settleIdle();
    postCommand(PATH_CANCEL, CONNECT_TIMEOUT_MS).catch(noop);
    notify('已取消录音');
  }

  function handleButtonClick(event, button) {
    if (event) {
      if (typeof event.preventDefault === 'function') event.preventDefault();
      if (typeof event.stopPropagation === 'function') event.stopPropagation();
    }
    if (state.disposed) return;
    var paneId = button ? getPaneId(button) : null;
    if (state.phase === 'recording') {
      stopAndInsert(paneId);
      return;
    }
    if (state.phase === 'stopping' || state.pending) return;
    startRecording(paneId);
  }

  function handleKeyDown(event) {
    if (!event) return;
    var isEscape = event.key === 'Escape' || event.key === 'Esc' || event.keyCode === 27;
    if (!isEscape) return;
    if (state.phase === 'recording') cancelRecording();
  }

  // ---------- 安装与清理 ----------

  function removeOwnNodes() {
    var nodes;
    try {
      nodes = document.querySelectorAll(OWN_SELECTOR);
    } catch (e) {
      return;
    }
    for (var i = nodes.length - 1; i >= 0; i--) removeNode(nodes[i]);
  }

  function dispose() {
    if (state.disposed) return 'ok';
    state.disposed = true;
    // 正在录音（或 start 请求还在途）时尽力通知服务端丢弃这段音频，失败也不阻塞清理。
    if (state.phase === 'recording' || (state.phase === 'idle' && state.pending)) {
      try {
        postCommand(PATH_CANCEL, CONNECT_TIMEOUT_MS).catch(noop);
      } catch (e) {}
    }
    clearTimeout(state.maxTimer);
    state.maxTimer = 0;
    clearTimeout(state.toastTimer);
    state.toastTimer = 0;
    if (state.interval) {
      clearInterval(state.interval);
      state.interval = 0;
    }
    for (var i = 0; i < state.bootTimers.length; i++) clearTimeout(state.bootTimers[i]);
    state.bootTimers = [];
    if (state.observer) {
      try {
        state.observer.disconnect();
      } catch (e) {}
    }
    state.observer = null;
    try {
      document.removeEventListener('keydown', handleKeyDown, true);
    } catch (e) {}
    try {
      document.removeEventListener('DOMContentLoaded', schedule);
    } catch (e) {}
    try {
      window.removeEventListener('resize', schedule);
    } catch (e) {}
    removeOwnNodes();
    state.toast = null;
    state.scheduled = false;
    settleIdle();
    if (installedInsertFn && window.insertVoiceText === installedInsertFn) {
      try {
        delete window.insertVoiceText;
      } catch (e) {
        window.insertVoiceText = undefined;
      }
    }
    installedInsertFn = null;
    window.__zcodePlusVoiceInstalled = false;
    return 'ok';
  }

  function install() {
    if (window.__zcodePlusVoiceInstalled) return false;
    window.__zcodePlusVoiceInstalled = true;

    installedInsertFn = function (text, paneId) {
      return insertVoiceText(text, paneId);
    };
    window.insertVoiceText = installedInsertFn;
    window.__zcodePlusVoiceDispose = dispose;

    installStyle();
    installObserver();

    try {
      document.addEventListener('keydown', handleKeyDown, true);
    } catch (e) {}
    try {
      window.addEventListener('resize', schedule);
    } catch (e) {}
    try {
      if (document.readyState === 'loading' && typeof document.addEventListener === 'function') {
        document.addEventListener('DOMContentLoaded', schedule);
      }
    } catch (e) {}

    // 启动时连补几扫：DOM 可能还没解析完；之后靠 observer + 定时器维持。
    state.bootTimers.push(setTimeout(schedule, 0));
    state.bootTimers.push(setTimeout(schedule, 1500));
    state.bootTimers.push(setTimeout(schedule, 5000));
    state.interval = setInterval(schedule, 2500);
    return true;
  }

  try {
    install();
    return 'ok';
  } catch (error) {
    // 安装失败时复位 flag，允许管理端下一次注入重试。
    try {
      window.__zcodePlusVoiceInstalled = false;
    } catch (e) {}
    return 'err:' + (error && error.message ? error.message : String(error));
  }
})();
