# SPDX-License-Identifier: MIT
pkgname=enzimos-dictation
pkgver=0.1.0
pkgrel=6
pkgdesc='enzimOS Voice Typing with Wayland insertion and clipboard delivery'
arch=('x86_64')
url='https://github.com/enz1m/enzimos-dictation'
license=('MIT')
depends=('glibc' 'gcc-libs' 'pipewire-audio' 'wl-clipboard')
makedepends=('cargo')
optdepends=('enzimos-streaming-speech: offline streaming English speech recognition'
            'enzimos-ai-gateway: use the configured inference speech model')
options=('!debug')

prepare() {
    cargo fetch --locked --manifest-path "$startdir/Cargo.toml"
}

build() {
    cargo build --frozen --release --manifest-path "$startdir/Cargo.toml" \
        --target-dir "${CARGO_TARGET_DIR:-$srcdir/cargo-target}"
}

package() {
    install -Dm755 "${CARGO_TARGET_DIR:-$srcdir/cargo-target}/release/enzimos-dictation" \
        "$pkgdir/usr/bin/enzimos-dictation"
    install -Dm644 "$startdir/deploy/enzimos-dictation.service" "$pkgdir/usr/lib/systemd/user/enzimos-dictation.service"
    install -Dm644 "$startdir/deploy/enzimos-dictation.desktop" "$pkgdir/usr/share/applications/enzimos-dictation.desktop"
    install -Dm644 "$startdir/deploy/enzimos-dictation.svg" "$pkgdir/usr/share/icons/hicolor/scalable/apps/enzimos-dictation.svg"
    install -Dm644 "$startdir/LICENSE" "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
