-- 云代理（www.ip3366.net）
--
-- 移植自 jhao104/proxy_pool 的 fetcher/sources/ip3366.py。两套列表：`stype=1`
-- 是国内、`stype=2` 是国外，页面结构一样，一行就是 `<td>IP</td><td>端口</td>`。
--
-- 原版每个 stype 只取第 1 页；这里用 `max_pages` 控制翻页（站点支持 `&page=N`），
-- 页与页之间 `sleep`，免得被当成爬虫。
--
-- 配置里的参数：
--   base_url   列表页地址（默认 http://www.ip3366.net/free/）
--   max_pages  每个 stype 翻几页，默认 1
--   delay      翻页间隔秒数，默认 1

local base = base_url or "http://www.ip3366.net/free/"
local pages = tonumber(max_pages) or 1
local delay = tonumber(delay) or 1

local result = {}
local first = true
for _, stype in ipairs({ 1, 2 }) do
  for page = 1, pages do
    local url = string.format("%s?stype=%d&page=%d", base, stype, page)
    local ok, body = pcall(fetch, url)
    if ok then
      for ip, port in body:gmatch("<td>(%d+%.%d+%.%d+%.%d+)</td>%s*<td>(%d+)</td>") do
        table.insert(result, { type = "http", ip = ip, port = tonumber(port) })
      end
    else
      log("ip3366 stype=" .. stype .. " page " .. page .. " could not be fetched")
    end
    if not first then sleep(delay) end
    first = false
  end
end
return result
