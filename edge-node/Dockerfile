# 构建阶段 (Rust)
FROM rust:1.75-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

# 极简生产运行阶段
FROM alpine:3.19
RUN apk add --no-cache ca-certificates libgcc
WORKDIR /opt/chongdian
COPY --from=builder /app/target/release/charging-space-edge /opt/chongdian/
COPY --from=builder /app/target/release/simulator /opt/chongdian/
EXPOSE 8081
ENTRYPOINT ["/opt/chongdian/charging-space-edge"]
