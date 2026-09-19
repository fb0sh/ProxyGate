-- SCDN 代理接口（proxy.scdn.io）
--
-- 移植自 jhao104/proxy_pool 的 fetcher/sources/scdn.py。原版打的是
-- `get_proxies.php`，返回的是一段 HTML 而不是数组：
--
--   {"table_html": "<tr><td class=\"cell-ip\">18.163.99.118</td><td>80</td>
--                   <td><span class='protocol-badge protocol-http'>HTTP</span></td>...",
--    "page": 1, "totalPages": 337}
--
-- 一页 100 条、一共几百页，所以默认只翻前几页（`max_pages`）：这个源本身不是
-- 按"质量"排的，翻更多页拿到的多半是同样命中的东西。协议从 class 里读
-- （`protocol-socks5` / `protocol-http`），比按显示文本读稳。
--
-- 配置里的参数：
--   base_url   接口地址模板，`%d` 依次是每页条数与页码
--              （默认 https://proxy.scdn.io/get_proxies.php?protocol=&country=&per_page=%d&page=%d）
--   per_page   每页条数，默认 100
--   max_pages  翻几页，默认 3
--   delay      翻页间隔秒数，默认 1

local template = base_url
  or "https://proxy.scdn.io/get_proxies.php?protocol=&country=&per_page=%d&page=%d"
local per_page = tonumber(per_page) or 100
local pages = tonumber(max_pages) or 3
local delay = tonumber(delay) or 1

local result = {}
for page = 1, pages do
  local url = string.format(template, per_page, page)
  local ok, body = pcall(fetch_json, url)
  if ok then
    local html = body.table_html or ""
    local found = 0
    for row in html:gmatch("<tr>(.-)</tr>") do
      local ip = row:match('<td class="cell%-ip">(%d+%.%d+%.%d+%.%d+)</td>')
      local port = row:match("<td>(%d+)</td>")
      if ip and port then
        local kind = "http"
        if row:find("protocol%-socks5", 1, false) then
          kind = "socks5"
        end
        table.insert(result, { type = kind, ip = ip, port = tonumber(port) })
        found = found + 1
      end
    end
    -- 空页说明翻到头了。
    if found == 0 then
      break
    end
  else
    log("scdn page " .. page .. " could not be fetched")
  end
  if page < pages then sleep(delay) end
end
return result
