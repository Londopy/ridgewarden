# Built by the release workflow from the static release binaries, one per architecture.
FROM scratch
ARG TARGETARCH
COPY ${TARGETARCH}/ridgewarden /ridgewarden
VOLUME ["/data", "/config"]
EXPOSE 8790
ENTRYPOINT ["/ridgewarden"]
CMD ["--config", "/config/ridgewarden.toml", "run"]
