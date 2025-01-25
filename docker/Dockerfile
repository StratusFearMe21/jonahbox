FROM debian:bookworm

RUN apt-get update && apt-get install -y libssl3 ca-certificates

WORKDIR /root
CMD /root/target/release/jonahbox
