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
        --bin subspace-chain-alerter && \
    mv target/*/subspace-chain-alerter subspace-chain-alerter && \
    rm -rf target

FROM ubuntu:24.04

COPY --from=0 /code/subspace-chain-alerter /subspace-chain-alerter

USER nobody:nogroup

# TODO:
# - when we have a local node, use it by default by dropping the --node-rpc-url argument
# - when multiple node support is fixed (#74), add the foundation node here as well
ENTRYPOINT ["/subspace-chain-alerter", "--node-rpc-url", "wss://rpc-0.mainnet.autonomys.xyz/ws"]
