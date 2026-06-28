FROM node:22-alpine

ENV CI=true
ENV TZ=Asia/Shanghai

RUN ln -snf /usr/share/zoneinfo/$TZ /etc/localtime \
    && echo $TZ > /etc/timezone \
    && apk add --no-cache \
        bash \
        file \
        patch \
        python3 \
        openssl \
        squashfs-tools

WORKDIR /app

COPY . .

RUN npm install

CMD ["npm", "run", "build"]