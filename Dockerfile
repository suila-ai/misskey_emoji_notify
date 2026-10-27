# syntax=docker/dockerfile:1

# ビルド。miapi の git 依存は cargo が内蔵の git 実装で取る。aws-lc-sys のビルドに C コンパイラが要る。
FROM rust:1-trixie AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src src
# registry・git・target をキャッシュに置き、バイナリだけを取り出す
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked \
    && cp target/release/misskey_emoji_notify /misskey_emoji_notify
# 状態の置き場。実行イメージには shell が無いので、ここで作って所有者を付けて写す
RUN mkdir /state

# 実行。glibc・libgcc・CA 証明書が入った distroless。TLS は rustls なので OpenSSL は要らない
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=builder /misskey_emoji_notify /usr/local/bin/misskey_emoji_notify
COPY --from=builder --chown=nonroot:nonroot /state /app/state
# 作業ディレクトリ。設定ファイル config.toml と、state_dir の既定（state）はここから引く
WORKDIR /app
ENTRYPOINT ["misskey_emoji_notify"]
