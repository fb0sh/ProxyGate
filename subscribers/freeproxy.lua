-- charlespikachu/freeproxy 的公开列表
-- （https://charlespikachu.github.io/freeproxy/proxies.json）
--
-- 这份 JSON 约 3 MB、1.8 万条，每条长这样：
--
--   {"ip": "109.95.220.45", "port": 8080, "protocol": "Http",
--    "country": "RU", "anonymity": "Elite", "speed": 299}
--
-- `source_url` 默认走 jsDelivr 上同一个仓库的镜像：内容一样，但实测快得多
-- （同一个沙箱里镜像 5.1s / 3.0 MB，GitHub Pages 直连要 170s 以上）。示例配置
-- 里显式写的是 Pages 地址——想要镜像就把 `source_url` 换成下面这行。
--   https://cdn.jsdelivr.net/gh/charlespikachu/freeproxy@master/proxies.json
--
-- 用 `fetch` + `gmatch` 扫字段，而不是 `fetch_json`：这些对象都是平的（没有任何
-- 嵌套的花括号），建一张 1.8 万行的 Lua 表纯属浪费——扫描省下的是几十 MB 的
-- 峰值内存，代价只是一次字符串遍历。
--
-- 协议字段混着大小写和组合写法（`Http`、`Http, Https`、`Socks4, Socks5`……），
-- 所以按"包含什么"判断，并且先判 socks5：`Socks4, Socks5` 该算 socks5。
-- 光有 socks4 的条目我们连不上（ProxyGate 不支持 socks4），直接跳过。
--
-- 配置里的参数：
--   source_url      列表地址（默认 jsDelivr 镜像）
--   wanted_country  只要这个国家的（两位代码，默认不限；列表里 LU/US/RU 最多）
--   limit           交给 ProxyGate 的条数上限（这份列表很长，示例配置里设了 500）

local body = fetch(source_url or "https://cdn.jsdelivr.net/gh/charlespikachu/freeproxy@master/proxies.json")

local result = {}
for object in body:gmatch("{[^{}]*}") do
  local ip = object:match('"ip"%s*:%s*"([^"]+)"')
  local port = object:match('"port"%s*:%s*(%d+)')
  local protocol = object:match('"protocol"%s*:%s*"([^"]*)"') or ""
  local where = object:match('"country"%s*:%s*"([^"]*)"')
  local lowered = protocol:lower()

  local kind = nil
  if lowered:find("socks5", 1, true) then
    kind = "socks5"
  elseif lowered:find("http", 1, true) then
    kind = "http"
  end

  if ip and port and kind and (wanted_country == nil or where == wanted_country) then
    table.insert(result, { type = kind, ip = ip, port = tonumber(port) })
  end
end
return result
