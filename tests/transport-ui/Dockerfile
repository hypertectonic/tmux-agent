FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8 TERM=xterm-256color
RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        lsof mosh openssh-client openssh-server procps python3 tmux util-linux \
    && useradd --create-home --home-dir "/home/agent" --shell /bin/bash agent \
    && passwd -d agent \
    && mkdir /run/sshd

ENTRYPOINT ["python3", "/opt/fixture/scenarios.py"]
