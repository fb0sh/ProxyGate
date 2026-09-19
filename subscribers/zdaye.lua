-- 站大爷（www.zdaye.com）的免费代理列表
--
-- HTML 列表，一页 20 条。国内段是 `/free/`（第 1 页没有数字后缀，之后
-- `/free/2/` … `/free/22/`），海外段是 `/free_haiwai/`（实测第 118 页还有 9 条）。
-- 同一段脚本靠 `base_url` 复用两次，配置里就是这么用的。
--
-- 这个站点前面挂着阿里云 WAF：实测连着抓十来个请求就会被拦成 405
-- （"您访问的URL有可能对网站造成安全威胁"），而且会持续一段时间。所以脚本做了
-- 三件事——页与页之间 `sleep`、单页失败用 `pcall` 跳过、默认只抓前几页。
-- 配置里再用 `via: pool` 从池子里的健康代理出去，被拉黑的就是代理的 IP。
--
-- 配置里的参数：
--   base_url   第 1 页地址（国内 https://www.zdaye.com/free/，
--              海外 https://www.zdaye.com/free_haiwai/）
--   max_pages  抓几页，默认 3
--   delay      页间间隔秒数，默认 2

local base = base_url or "https://www.zdaye.com/free/"
local pages = tonumber(max_pages) or 3
local delay = tonumber(delay) or 2

local result = {}
for page = 1, pages do
  local url = page == 1 and base or (base .. page .. "/")
  local ok, body = pcall(fetch, url)
  if ok then
    local found = 0
    for row in body:gmatch('<ul class="ul%-row">(.-)</ul>') do
      local ip = row:match('class="proxy_ip">([^<]+)<')
      -- 注意别写成 `Port[：:](%d+)`：Lua 的模式是**按字节**匹配的，
      -- 把全角冒号放进字符类只会吃掉它的第一个字节，`%d+` 随即失败。
      local port = row:match("Port[^%d]*(%d+)")
      if ip and port then
        local protocol = row:match("protocol_span[^>]*>%s*([A-Za-z0-9]+)") or "http"
        table.insert(result, { type = protocol, ip = ip, port = tonumber(port) })
        found = found + 1
      end
    end
    -- 空页说明翻到头了，或者这一页被 WAF 换成了别的页面。
    if found == 0 then
      break
    end
    if page < pages then
      sleep(delay)
    end
  else
    log("zdaye page " .. page .. " could not be fetched")
  end
end
return result
