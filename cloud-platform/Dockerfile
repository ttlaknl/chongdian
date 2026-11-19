# Frontend static assets
FROM node:22-alpine AS frontend-builder
WORKDIR /workspace
COPY frontend/package.json frontend/pnpm-lock.yaml ./frontend/
RUN corepack enable && cd frontend && pnpm install --frozen-lockfile
COPY frontend ./frontend
COPY cloud-platform/src/main/resources/static/index.html ./cloud-platform/src/main/resources/static/index.html
COPY cloud-platform/src/main/resources/static/admin.html ./cloud-platform/src/main/resources/static/admin.html
COPY edge-node/static/admin.html ./edge-node/static/admin.html
RUN cd frontend && pnpm build

# 构建镜像阶段 (Java 21)
FROM maven:3.9-eclipse-temurin-21-alpine AS builder
WORKDIR /app
COPY cloud-platform/pom.xml .
COPY cloud-platform/src ./src
COPY --from=frontend-builder /workspace/cloud-platform/src/main/resources/static/vendor ./src/main/resources/static/vendor
RUN mvn clean package -DskipTests

# 生产运行镜像阶段
FROM eclipse-temurin:21-jre-alpine
WORKDIR /app
COPY --from=builder /app/target/charging-cloud-0.1.0.jar app.jar
RUN addgroup -S app && adduser -S app -G app && mkdir -p /app/data/cloud_store/firmware && chown -R app:app /app
USER app
EXPOSE 8080
ENTRYPOINT ["java", "-jar", "app.jar"]
