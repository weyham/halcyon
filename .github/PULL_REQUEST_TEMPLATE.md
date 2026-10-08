## 改了什么

<!-- 一句话说明动机与范围 -->

## 规则影响

- [ ] 不影响正常配对的 `function_call` 与 `function_call_output`
- [ ] 不改动模型消息与 reasoning 内容
- [ ] 解析异常时退化为原样透传加日志
- [ ] 未涉及 secrets（Authorization 只透传，不读取也不落盘）

## 验证

- [ ] `scripts\check.ps1` 全绿（含本次新增/更新的测试）
- [ ] 若涉及改写规则：已同步 `docs/design.md`
