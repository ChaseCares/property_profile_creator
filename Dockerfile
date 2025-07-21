FROM rust:1-slim-bookworm AS builder

WORKDIR /app

RUN apt-get update && apt-get install -y git libssl-dev pkg-config
RUN git clone https://github.com/ChaseCares/property_profile_creator.git .
RUN cargo build --release


FROM debian:stable-slim

RUN apt-get update && apt-get install -y libssl3 && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/app

COPY --from=builder /app/target/release/property_profile_creator .

EXPOSE 8080

CMD ["./property_profile_creator"]
