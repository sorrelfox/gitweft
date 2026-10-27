# The architecture the packages are built for: GITWEFT_ARCH (x86_64 or aarch64), else this
# machine's. Sourced by scripts/dist/*.sh. Their containers run as that architecture
# (--platform), so on a machine of the other one they need QEMU user-mode emulation
# (Fedora: qemu-user-static).
arch=${GITWEFT_ARCH:-$(uname -m)}
case $arch in
    x86_64 | amd64) arch=x86_64 deb_arch=amd64 ;;
    aarch64 | arm64) arch=aarch64 deb_arch=arm64 ;;
    *) echo "$0: can't build for $arch (x86_64 or aarch64)" >&2; exit 1 ;;
esac
platform=linux/$deb_arch
