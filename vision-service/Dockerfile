FROM python:3.11-slim

WORKDIR /app

RUN apt-get update && apt-get install -y --no-install-recommends \
    libgl1 \
    libglib2.0-0 \
    ffmpeg \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt -i https://mirrors.aliyun.com/pypi/simple/ --trusted-host mirrors.aliyun.com

COPY plate_recognition ./plate_recognition
COPY weights ./weights
COPY config.py plate_recognizer.py ai_engine.py rtsp_worker.py main.py ./

EXPOSE 8090

CMD ["python", "-u", "main.py"]
