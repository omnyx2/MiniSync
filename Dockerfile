FROM alpine:latest
COPY dist/minisync-linux-arm64 /usr/local/bin/minisync
RUN chmod +x /usr/local/bin/minisync
RUN mkdir /data
ENTRYPOINT ["minisync"]
CMD ["/data", "0.0.0.0:9000"]
