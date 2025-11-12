FROM --platform=$BUILDPLATFORM ubuntu:24.04

ARG PROFILE=production
ARG RUSTFLAGS
# Incremental compilation here isn't helpful
ENV CARGO_INCREMENTAL=0

WORKDIR /code

RUN \
    apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        llvm \
        clang

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none

# Up until this line all Rust images in this repo should be the same to share the same layers

COPY . /code


RUN /root/.cargo/bin/cargo build \
        --locked \
        --profile $PROFILE \
        --bin alerter && \
    mv target/*/alerter /alerter && \
    rm -rf target

FROM ubuntu:24.04

RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive  apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=0 /alerter /alerter

COPY --from=0 /code/alerter/networks.toml /networks.toml

USER nobody:nogroup

ENTRYPOINT [ "/alerter" ]
