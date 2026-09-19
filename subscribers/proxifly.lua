-- Proxifly 免费代理列表（proxifly.dev）
--
-- 移植自 jhao104/proxy_pool 的 fetcher/sources/proxifly.py。原版从 jsDelivr 上
-- 拉全量 JSON（约 5 MB、1.8 万条），只留 `geolocation.country == "CN"` 且
-- `protocol == "http"` 的那些（实测 346 条），够用又便宜。
--
-- 这份 JSON 的对象**有嵌套**（`geolocation` 是个子对象），所以没法像 freeproxy
-- 那样用 `{[^{}]*}` 一刀切。这里的做法是：找到 `"proxy": "..."` 之后在**同一个
-- 对象体内**（到下一个 `"proxy"` 之前）找 `"country"`——字段顺序是 proxy 在前、
-- country 在后，实测稳定。于是不必把这 1.8 万条建成 Lua 表，峰值内存小一个量级。
--
-- 配置里的参数：
--   source_url      接口地址（默认 jsDelivr 上的 all/data.json）
--   wanted_country  只保留这个国家的条目（默认 CN；设成 "" 表示不限）
--   window_size     同一个对象体的最大字节数，默认 512

local body = fetch(source_url or "https://cdn.jsdelivr.net/gh/proxifly/free-proxy-list@main/proxies/all/data.json")

local wanted = wanted_country
if wanted == nil then
  wanted = "CN"
end
local window_size = tonumber(window_size) or 512

local result = {}
local pos = 1
while true do
  local _, stop, url = body:find('"proxy"%s*:%s*"([^"]+)"', pos)
  if not stop then
    break
  end
  local window = body:sub(stop, stop + window_size - 1)
  local next_object = window:find('"proxy"', 1, true)
  if next_object then
    window = window:sub(1, next_object - 1)
  end

  local where = window:match('"country"%s*:%s*"([A-Z][A-Z])"')
  -- `proxy` 字段本身带的 scheme 就是协议：http/https 归一成 http，socks5 交给
  -- ProxyGate 变成 socks5h，socks4 直接不要。
  local scheme = url:match("^(%a+)://")
  local kind = nil
  if scheme == "http" or scheme == "https" then
    kind = "http"
  elseif scheme == "socks5" or scheme == "socks5h" then
    kind = "socks5"
  end

  if kind and (wanted == "" or where == wanted) then
    local ip, port = url:match("^%a+://([^:@/]+):(%d+)$")
    if ip and port then
      table.insert(result, { type = kind, ip = ip, port = tonumber(port) })
    end
  end
  pos = stop
end
return result
