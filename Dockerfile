# plant-broker is deployed separately from plant on purpose: this image is the
# only vaultr artifact that receives the seal-store IRSA role.
FROM docker.io/library/rust:1.96-alpine AS build
# gcc, not just the musl headers: the TokenReview leg (#1419) pulls in rustls,
# whose `ring` provider compiles C. The workspace resolves rustls to `ring` and
# not `aws-lc-rs` — check `Cargo.lock` before assuming cmake is needed here.
RUN apk add --no-cache musl-dev gcc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p plant-broker

# The broker shells out to the AWS CLI so the same credential resolver works
# with IRSA in athens, SSO during local proving, and env credentials in tests.
# Pin the image; `latest` would make the credential path change without review.
FROM public.ecr.aws/aws-cli/aws-cli:2.36.21
COPY --from=build /src/target/release/plant-broker /usr/local/bin/plant-broker
ENV HOME=/tmp
ENTRYPOINT ["/usr/local/bin/plant-broker"]
