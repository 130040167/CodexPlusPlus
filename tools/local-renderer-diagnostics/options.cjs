function monitorOptions(args, now = Date.now()) {
  const until = args.find(arg => arg.startsWith('--until='));
  const daysArgument = args.find(arg => arg.startsWith('--days='));
  const stateArgument = args.find(arg => arg.startsWith('--state='));
  const days = daysArgument ? Number(daysArgument.slice('--days='.length)) : 1;
  if (!Number.isFinite(days) || days <= 0 || days > 7) throw new Error('days must be between 0 and 7');
  const expiresAt = until ? Date.parse(until.slice('--until='.length)) : now + days * 86400000;
  if (!Number.isFinite(expiresAt) || expiresAt > now + 8 * 86400000) throw new Error('invalid monitoring deadline');
  return {
    expiresAt,
    maxBytes: 128 * 1024 * 1024,
    staleMs: 90 * 1000,
    statePath: stateArgument ? stateArgument.slice('--state='.length) : null,
    expired: expiresAt <= now,
  };
}
module.exports = monitorOptions;
