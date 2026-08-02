# App .so 编译修复

## 问题
`session.rs` `recv_raw_with_fds` 有 E0502 borrow 错误 + 重复 fds 提取

## 修复
`iov` 用 block 限定生命周期，重复代码删除：

```rust
// 修复前 (lines 145-189):
let mut iov = [IoSliceMut::new(buf)];  // iov 一直活着到函数结束
let msg = recvmsg(...)?;
// ... 第一次 fds 提取 ...
// ... buf[0] 访问 ← 这里 iov 还活着，borrow 冲突
// ... 第二次 fds 提取 ← 重复代码

// 修复后:
let (n_bytes, fds) = {
    let mut iov = [IoSliceMut::new(buf)];
    let msg = recvmsg(...)?;
    let fds = extract_fds(msg);
    (msg.bytes, fds)
};  // iov dropped here, buf freed

if n_bytes < 4 { ... }
let msg_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
let data = buf[4..4 + msg_len].to_vec();
Ok((data, fds))  // 直接返回第一次提取的 fds
```
