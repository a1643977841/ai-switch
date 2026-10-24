export const ACCOUNT_PROXY_URL_CONFIG_KEY = "proxy_url";

const SUPPORTED_PROXY_SCHEMES = new Set(["http:", "https:", "socks5:", "socks5h:"]);

export function readProxyUrlFromConfig(config: Record<string, unknown>): string {
  const raw = config[ACCOUNT_PROXY_URL_CONFIG_KEY];
  return typeof raw === "string" ? raw.trim() : "";
}

export function writeProxyUrlToConfig(
  config: Record<string, unknown>,
  proxyUrl: string,
): Record<string, unknown> {
  const next = { ...config };
  const trimmed = proxyUrl.trim();
  if (trimmed) {
    next[ACCOUNT_PROXY_URL_CONFIG_KEY] = trimmed;
  } else {
    delete next[ACCOUNT_PROXY_URL_CONFIG_KEY];
  }
  return next;
}

/** Client-side mirror of the backend validation, so a bad address never
 * reaches the save request. Empty means "follow the app-wide proxy". */
export function proxyUrlError(proxyUrl: string): string | null {
  const trimmed = proxyUrl.trim();
  if (!trimmed) {
    return null;
  }
  if (/\s/.test(trimmed)) {
    return "代理地址不能包含空格。";
  }
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return "代理地址格式无效，需形如 http://127.0.0.1:7890。";
  }
  if (!SUPPORTED_PROXY_SCHEMES.has(parsed.protocol)) {
    return "仅支持 http、https、socks5、socks5h 代理。";
  }
  if (!parsed.hostname) {
    return "代理地址缺少主机名。";
  }
  return null;
}
