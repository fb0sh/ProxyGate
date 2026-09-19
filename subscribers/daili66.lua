-- 66代理（api.66daili.com）
--
-- 移植自 jhao104/proxy_pool 的 fetcher/sources/daili66.py。接口直接吐 JSON：
--
--   {"code": 0, "total": 60, "data": [{"ip": "8.213.215.187", "port": "194", "protocol": "HTTP"}]}
--
-- 一次 60 条、没有分页。`protocol` 那一项是 HTTP/HTTPS/SOCKS5 之类的字符串，
-- 直接当 `type` 交给 ProxyGate 归一化（HTTPS→http、SOCKS5→socks5h、SOCKS4 丢弃）。
--
-- 配置里的参数：
--   api_url  接口地址（默认就是下面这个）

local body = fetch_json(api_url or "http://api.66daili.com/?format=json")

local result = {}
for _, item in ipairs(body.data or {}) do
  if item.ip and item.port then
    table.insert(result, {
      type = item.protocol or "http",
      ip = item.ip,
      port = tonumber(item.port),
    })
  end
end
return result
