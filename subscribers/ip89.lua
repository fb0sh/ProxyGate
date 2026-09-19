-- 89免费代理（www.89ip.cn）
--
-- 移植自 jhao104/proxy_pool 的 fetcher/sources/ip89.py。列表页是
-- `/index_1.html`、`/index_2.html`……原版只取第 1 页（一页 40 条），这里用
-- `max_pages` 控制翻几页。
--
-- 行结构（注意单元格里有换行和制表符，所以用 `%s*` 放宽）：
--
--   <tr><td>\n\t\t\t47.113.224.182\t\t</td><td>\n\t\t\t9999\t\t</td><td>浙江省杭州市</td>...
--
-- 配置里的参数：
--   base_url   列表页地址模板，`%d` 会被替换成页码（默认 https://www.89ip.cn/index_%d.html）
--   max_pages  翻几页，默认 1
--   delay      翻页间隔秒数，默认 1

local template = base_url or "https://www.89ip.cn/index_%d.html"
local pages = tonumber(max_pages) or 1
local delay = tonumber(delay) or 1

local result = {}
for page = 1, pages do
  local url = string.format(template, page)
  local ok, body = pcall(fetch, url)
  if ok then
    for ip, port in body:gmatch(
      "<td[^>]*>%s*(%d+%.%d+%.%d+%.%d+)%s*</td>%s*<td[^>]*>%s*(%d+)%s*</td>"
    ) do
      table.insert(result, { type = "http", ip = ip, port = tonumber(port) })
    end
  else
    log("ip89 page " .. page .. " could not be fetched")
  end
  if page < pages then sleep(delay) end
end
return result
