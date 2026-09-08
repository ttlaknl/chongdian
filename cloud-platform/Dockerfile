# 构建镜像阶段 (Java 21)
FROM maven:3.9-eclipse-temurin-21-alpine AS builder
WORKDIR /app
COPY pom.xml .
COPY src ./src
RUN mvn clean package -DskipTests

# 生产运行镜像阶段
FROM eclipse-temurin:21-jre-alpine
WORKDIR /app
COPY --from=builder /app/target/charging-cloud-0.1.0.jar app.jar
EXPOSE 8080
ENTRYPOINT ["java", "-jar", "app.jar"]
