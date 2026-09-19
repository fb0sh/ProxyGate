-- 小幻代理（ip.ihuan.me）
--
-- 移植自 jhao104/proxy_pool 的 fetcher/sources/ihuan.py。原版先 GET 一次首页拿
-- cookie、再 GET 一次拿列表；实测直接 GET 一次就有数据（cookie 只在部分路径上
-- 才被检查），所以这里不额外多打一个请求。
--
-- 每一行的形状：
--
--   <tr><td><a href="/check.html?proxy=NDMuMTM2LjY5LjEzMDo4MA=="><img src="/flag/CN.svg">43.136.69.130</a></td><td>80</td>...
--
-- 关键点：href 里的 base64 尾巴、flag 的路径都夹在标签里，直接对整行匹配数字会
-- 被它们干扰。所以先把标签全部换成空格，再取**第一对** "地址 端口"。一页 18 条
-- 左右，没有分页。
--
-- 配置里的参数：
--   base_url  列表页地址（默认 https://ip.ihuan.me/）

local body = fetch(base_url or "https://ip.ihuan.me/")

local result = {}
for row in body:gmatch("<tr>(.-)</tr>") do
  local plain = row:gsub("<[^>]*>", " ")
  local ip, port = plain:match("(%d+%.%d+%.%d+%.%d+)%s+(%d+)")
  if ip and port then
    table.insert(result, { type = "http", ip = ip, port = tonumber(port) })
  end
end
return result
