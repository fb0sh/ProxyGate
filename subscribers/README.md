# subscribers/

现成的订阅源：**一个站点一个文件**。`config.example.yaml` 里就是这套用法：

```yaml
subscribers:
  - name: ip89
    timeout: 30s
    max_pages: 3 # 除 ProxyGate 自己的键以外，都会变成脚本里的全局变量
    delay: 1
    lua_file: subscribers/ip89.lua
```

`lua_file` 的相对路径按**配置文件所在目录**解析，所以把 `config.yaml` 和这个目录
放在一起就行（从任何工作目录启动都一样）。不想用文件也可以把脚本内联在
`lua_code:` 里——两者二选一。

## 契约

脚本自己去取数据、自己翻页、自己拼装，最后 **return 一组代理表**：

```lua
return {
  { type = "http",    ip = "1.2.3.4", port = 8080 },
  { type = "socks5",  ip = "5.6.7.8", port = 1080, auth = "user:pass" },
}
```

| 字段 | 说明 |
| --- | --- |
| `type` | `http` / `https` / `ssl` → `http`；`socks5` / `socks5h` / `socks` → `socks5h`；`socks4` 或认不出的名字会被跳过 |
| `ip` | 主机名也行；IPv6 会自动套方括号 |
| `port` | 数字或字符串；不写就用协议的默认端口 |
| `auth` | 可选，`user:password`，会做百分号编码 |

条目也可以直接写成字符串（`"socks5h://user:pass@1.2.3.4:1080"`、`"1.2.3.4:8080"`）。

脚本里能用的东西：`fetch(url[, headers])`、`fetch_json(url[, headers])`、
`json_encode` / `json_decode`、`sleep(秒)`、`log(...)` / `print(...)`。
沙箱不加载 `io`/`os`/`package`/`debug`，`dofile`/`loadfile`/`load`/`require` 也被摘掉，
`fetch` 是唯一的出口；整段脚本受 `timeout` 限制（`while true do end` 会被掐断）。
细节见仓库 README 的「用 Lua 写 subscriber」，或运行中的 `GET /help`。

## 来源

这些站点的地址、翻页方式与字段解析**移植自
[jhao104/proxy_pool](https://github.com/jhao104/proxy_pool)（MIT）** 的
`fetcher/sources/*.py`：那边一个源一个 Python 文件，这里一个源一个 Lua 文件，页面数、
间隔、过滤条件都提到配置里。原始版权归 JHao 与该项目贡献者所有。

新增一个源不必改 ProxyGate：写一个 `.lua` 丢进来，在配置里加一段就行。
`cargo test --lib every_shipped_subscriber_compiles_in_the_sandbox` 会把这里的每个
文件过一遍沙箱编译，语法错误不会等到第一次刷新才暴露。
