-- Roundproxies（roundproxies.com/free-proxy-list）
--
-- 移植自 jhao104/proxy_pool 的 fetcher/sources/roundproxies.py。JSON 接口一次
-- 50 条，字段里有 `protocols` 数组（socks5 / http / https）：
--
--   {"data": [{"ip": "65.21.252.66", "port": "10812", "protocols": ["socks5"], ...}]}
--
-- 这个源是海外的，命中率比国内那几个高，值得多翻几页。
--
-- 配置里的参数：
--   base_url   接口地址模板，`%d` 依次是每页条数与页码
--              （默认 https://roundproxies.com/api/get-free-proxies/?limit=%d&page=%d&sort_by=lastChecked&sort_type=desc）
--   per_page   每页条数，默认 50（接口上限以内）
--   max_pages  翻几页，默认 1
--   delay      翻页间隔秒数，默认 1

local template = base_url
  or "https://roundproxies.com/api/get-free-proxies/?limit=%d&page=%d&sort_by=lastChecked&sort_type=desc"
local per_page = tonumber(per_page) or 50
local pages = tonumber(max_pages) or 1
local delay = tonumber(delay) or 1

local result = {}
for page = 1, pages do
  local url = string.format(template, per_page, page)
  local ok, body = pcall(fetch_json, url)
  if ok then
    local rows = body.data or {}
    for _, item in ipairs(rows) do
      if item.ip and item.port then
        table.insert(result, {
          type = (item.protocols and item.protocols[1]) or "http",
          ip = item.ip,
          port = tonumber(item.port),
        })
      end
    end
    -- 空页说明翻到头了。
    if #rows == 0 then
      break
    end
  else
    log("roundproxies page " .. page .. " could not be fetched")
  end
  if page < pages then sleep(delay) end
end
return result
