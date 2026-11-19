# 构建阶段 (Rust)
FROM rust:1.95-alpine AS builder
RUN apk add --no-cache musl-dev g++
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --release

# 极简生产运行阶段
FROM alpine:3.19
RUN apk add --no-cache ca-certificates libgcc libstdc++
WORKDIR /opt/chongdian
COPY --from=builder /app/target/release/charging-space-edge /opt/chongdian/
COPY --from=builder /app/target/release/simulator /opt/chongdian/
RUN addgroup -S edge && adduser -S edge -G edge && mkdir -p /data/edge && chown -R edge:edge /opt/chongdian /data/edge
USER edge
EXPOSE 8081
ENTRYPOINT ["/opt/chongdian/charging-space-edge"]
