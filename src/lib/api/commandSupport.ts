export const desktopOnlyCommands = [
  "enable_client_direct_mode",
  "start_codex_oauth",
  "get_codex_oauth_status",
  "cancel_codex_oauth",
  // Reads this machine's `~/.codex/auth.json`; a browser session has no such file.
  "get_codex_login_state",
  "open_route_proxy_https_certificate_dir",
  "open_session_terminal",
  "save_route_credential_export",
  "save_route_proxy_diagnostics_export",
] as const;

export function isDesktopOnlyCommand(command: string) {
  return (desktopOnlyCommands as readonly string[]).includes(command);
}
