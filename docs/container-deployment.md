# 容器侧部署指南

## 前置条件

Droidspaces 将宿主机 socket 目录 bind mount 进容器：

```yaml
mounts:
  - /dev/socket:/dev/socket:shared
```

## 构建

```bash
# 容器内
apt install libwlroots-dev libwayland-dev libdrm-dev
cd test-compositor && make
sudo cp wl-android-compositor /usr/local/bin/
```

或一键脚本：

```bash
bash <(curl -s https://raw.githubusercontent.com/ShiinaKagari/wl-android/main/docs/scripts/container-build.sh)
```

## 运行

```bash
wl-android-compositor &
WAYLAND_DISPLAY=wl-android-0 your-app
```

合成器默认连接 `/run/land.sock`，可通过 `LAND_SOCKET` 环境变量覆盖。

## 验证

```bash
ls -la /run/land.sock   # 容器内 socket
ls -la /data/local/tmp/land.sock  # 宿主机 socket (bind mount)
```
