FROM python:3.11-slim

WORKDIR /app

RUN sed -i 's/deb.debian.org/mirrors.aliyun.com/g' /etc/apt/sources.list.d/debian.sources 2>/dev/null || sed -i 's/deb.debian.org/mirrors.aliyun.com/g' /etc/apt/sources.list && \
    apt-get update && apt-get install -y --no-install-recommends \
    libgl1 \
    libglib2.0-0 \
    ffmpeg \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt -i https://mirrors.aliyun.com/pypi/simple/ --trusted-host mirrors.aliyun.com

COPY backends ./backends
COPY plate_recognition ./plate_recognition
COPY scripts ./scripts
COPY weights ./weights
COPY config.py plate_recognizer.py ai_engine.py geometry.py plate_postprocess.py space_tracker.py rtsp_worker.py main.py ./

EXPOSE 8090

CMD ["python", "-u", "main.py"]
