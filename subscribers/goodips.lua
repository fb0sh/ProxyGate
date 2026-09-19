-- 谷德代理（www.goodips.com）
--
-- 移植自 jhao104/proxy_pool 的 fetcher/sources/goodips.py。首页就是一张列表，
-- 一页 15 条左右，没有分页。每一行长这样：
--
--   <div class="table-list"><ul class="flex">
--     <li ...>39.104.23.154</li>      -- IP
--     <li ...>9080</li>               -- 端口
--     <li ...><span>北京市 阿里云</span></li>
--     <li ...>高匿</li><li class="color-01">HTTP</li>
--     ...
--
-- 所以按 <li> 的**顺序**取：第一个长得像 IP 的当地址，紧跟其后的第一个纯数字
-- 当端口。这样写比写死"第 1 个 li / 第 2 个 li"抗改版：站点偶尔会插一列。
--
-- 配置里的参数：
--   base_url  列表页地址（默认 https://www.goodips.com/）

local body = fetch(base_url or "https://www.goodips.com/")

local result = {}
for block in body:gmatch('<div class="table%-list"[^>]*>(.-)</div>') do
  local ip, port = nil, nil
  for value in block:gmatch("<li[^>]*>%s*([^<]-)%s*</li>") do
    if not ip and value:match("^%d+%.%d+%.%d+%.%d+$") then
      ip = value
    elseif ip and not port and value:match("^%d+$") then
      port = value
      break
    end
  end
  if ip and port then
    table.insert(result, { type = "http", ip = ip, port = tonumber(port) })
  end
end
return result
