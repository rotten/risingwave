FROM ubuntu:22.04 AS base

ARG LANG=en_US.utf8
ENV LANG=$LANG

SHELL ["/bin/bash", "-o", "pipefail", "-c"]
RUN \
    apt-get update -q -y && \
    apt-get upgrade -q -y && \
    apt-get install -q --no-install-recommends -y \
        build-essential \
        ca-certificates \
        libsasl2-dev \
        locales \
        openjdk-11-jdk && \
    rm -rf /var/lib/apt/lists/* && \
    locale-gen ${LANG}


#######################
FROM base AS builder

SHELL ["/bin/bash", "-o", "pipefail", "-c"]
RUN \
    apt-get update -q -y && \
    apt-get install -q --no-install-recommends -y \
        cmake \
        curl \
        libprotobuf-dev \
        lld \
        maven \
        protobuf-compiler \
        unzip && \
    rm -rf /var/lib/apt/lists/* && \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- --no-modify-path --default-toolchain none -y && \
    mkdir -p /risingwave

WORKDIR /risingwave

COPY ./ /risingwave

ENV PATH /root/.cargo/bin/:$PATH

ENV IN_CONTAINER=1

ARG GIT_SHA
ENV GIT_SHA=$GIT_SHA

SHELL ["/bin/bash", "-o", "pipefail", "-c"]
RUN curl -LO https://github.com/risingwavelabs/risingwave/archive/refs/heads/dashboard-artifact.zip && \
    unzip dashboard-artifact.zip && \
    mv risingwave-dashboard-artifact /risingwave/ui && \
    rm dashboard-artifact.zip && \
    rustup set profile minimal && \
    # We need to add the `rustfmt` dependency, otherwise `risingwave_pb` will not compile \
    rustup component add rustfmt && \
    cargo fetch -v && \
    # build the executables \
    cargo build -p risingwave_cmd_all --release --features "rw-static-link" && \
    mkdir -p /risingwave/bin && \
    mv /risingwave/target/release/risingwave /risingwave/bin/ && \
    mv /risingwave/target/release/risingwave.dwp /risingwave/bin/ && \
    cp ./target/release/build/tikv-jemalloc-sys-*/out/build/bin/jeprof /risingwave/bin/ && \
    chmod +x /risingwave/bin/jeprof && \
    mkdir -p /risingwave/lib && \
    cargo clean

# build the connectors
WORKDIR /risingwave/java
RUN mvn -B package -Dmaven.test.skip=true -Djava.binding.release=true && \
    mkdir -p /risingwave/bin/connector-node && \
    tar -zxvf /risingwave/java/connector-node/assembly/target/risingwave-connector-1.0.0.tar.gz -C /risingwave/bin/connector-node


#######################
FROM base AS risingwave

LABEL org.opencontainers.image.source https://github.com/risingwavelabs/risingwave

SHELL ["/bin/bash", "-o", "pipefail", "-c"]
RUN apt-get update -q -y && \
    apt-get install -q --no-install-recommends -y gdb && \
    rm -rf /var/lib/apt/lists/* && \
    rm -rf /var/lib/{apt,dpkg,cache,log}/ && \
    mkdir -p /risingwave/bin/connector-node && mkdir -p /risingwave/lib

COPY --from=builder /risingwave/bin/risingwave /risingwave/bin/risingwave
COPY --from=builder /risingwave/bin/risingwave.dwp /risingwave/bin/risingwave.dwp
COPY --from=builder /risingwave/bin/connector-node /risingwave/bin/connector-node
COPY --from=builder /risingwave/ui /risingwave/ui
COPY --from=builder /risingwave/bin/jeprof /usr/local/bin/jeprof

# Set default playground mode to docker-playground profile
ENV PLAYGROUND_PROFILE docker-playground
# Set default dashboard UI to local path instead of github proxy
ENV RW_DASHBOARD_UI_PATH /risingwave/ui
# Set default connector libs path
ENV CONNECTOR_LIBS_PATH /risingwave/bin/connector-node/libs

ENTRYPOINT [ "/risingwave/bin/risingwave" ]
CMD [ "playground" ]
