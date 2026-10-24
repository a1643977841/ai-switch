import { describe, expect, it } from "vitest";
import {
  ACCOUNT_PROXY_URL_CONFIG_KEY,
  proxyUrlError,
  readProxyUrlFromConfig,
  writeProxyUrlToConfig,
} from "../src/lib/accountProxy";

describe("accountProxy", () => {
  it("reads only trimmed string values", () => {
    expect(readProxyUrlFromConfig({})).toBe("");
    expect(readProxyUrlFromConfig({ proxy_url: 123 })).toBe("");
    expect(readProxyUrlFromConfig({ proxy_url: "  http://127.0.0.1:7890  " })).toBe(
      "http://127.0.0.1:7890",
    );
  });

  it("writes and removes the proxy field", () => {
    const written = writeProxyUrlToConfig({ base_url: "https://api.example.com" }, "  http://p:1 ");
    expect(written[ACCOUNT_PROXY_URL_CONFIG_KEY]).toBe("http://p:1");
    expect(written.base_url).toBe("https://api.example.com");

    expect(writeProxyUrlToConfig(written, "   ")).not.toHaveProperty(
      ACCOUNT_PROXY_URL_CONFIG_KEY,
    );
  });

  it("accepts the supported proxy schemes", () => {
    for (const value of [
      "http://127.0.0.1:7890",
      "https://proxy.example.com:8443",
      "socks5://10.0.0.1:1080",
      "socks5h://user:pass@proxy.example.com:1080",
    ]) {
      expect(proxyUrlError(value)).toBeNull();
    }
    expect(proxyUrlError("   ")).toBeNull();
  });

  it("explains why an address is rejected", () => {
    expect(proxyUrlError("ftp://127.0.0.1:21")).toBe(
      "仅支持 http、https、socks5、socks5h 代理。",
    );
    expect(proxyUrlError("127.0.0.1:7890")).toContain("格式无效");
    expect(proxyUrlError("http://127.0.0.1:78 90")).toContain("空格");
  });
});
