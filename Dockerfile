ARG APP_NAME=ab-av1

FROM rust:1.66-buster AS build
ARG APP_NAME
ENV APP_NAME=${APP_NAME}

WORKDIR /usr/src/${APP_NAME}
COPY . .

RUN cargo install --path .

FROM debian:buster-slim
ARG APP_NAME
ENV APP_NAME=${APP_NAME}

COPY --from=build /usr/local/cargo/bin/${APP_NAME} /usr/local/bin/${APP_NAME}

# install dependencies
RUN apt-get update && apt-get install -y \
  ffmpeg \
  && rm -rf /var/lib/apt/lists/* && apt autoremove -y && apt clean

# ENTRYPOINT /bin/bash -c ${APP_NAME}
ENTRYPOINT ["ab-av1"]
