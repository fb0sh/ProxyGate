-- 快代理（www.kuaidaili.com）
--
-- 移植自 jhao104/proxy_pool 的 fetcher/sources/kuaidaili.py。两条列表：
-- `inha`（国内高匿）和 `intr`（国内普通），行结构一样：
--
--   <tr><td class="kdl-table-cell">113.68.83.242</td><td class="kdl-table-cell">8090</td>
--     <td class="kdl-table-cell">HTTP</td>...
--
-- 原版在两次请求之间 `sleep(1)`，注释写着"必须 sleep 不然第二条请求不到数据"，
-- 这里保留这个节奏（`delay` 可调）。
--
-- 配置里的参数：
--   base_url   列表页地址模板，两个 `%s`/`%d` 分别是 inha|intr 和页码
--              （默认 https://www.kuaidaili.com/free/%s/%d/）
--   max_pages  每条列表翻几页，默认 1
--   delay      请求间隔秒数，默认 1

local template = base_url or "https://www.kuaidaili.com/free/%s/%d/"
local pages = tonumber(max_pages) or 1
local delay = tonumber(delay) or 1

local result = {}
local first = true
for _, kind in ipairs({ "inha", "intr" }) do
  for page = 1, pages do
    local url = string.format(template, kind, page)
    local ok, body = pcall(fetch, url)
    if ok then
      for ip, port in body:gmatch(
        '<td class="kdl%-table%-cell">%s*(%d+%.%d+%.%d+%.%d+)%s*</td>%s*<td class="kdl%-table%-cell">%s*(%d+)%s*</td>'
      ) do
        table.insert(result, { type = "http", ip = ip, port = tonumber(port) })
      end
    else
      log("kuaidaili " .. kind .. " page " .. page .. " could not be fetched")
    end
    if not first then sleep(delay) end
    first = false
  end
end
return result
