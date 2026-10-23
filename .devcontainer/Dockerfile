FROM mcr.microsoft.com/devcontainers/rust:2-1-trixie

# Claude Code は vscode ユーザーの ~/.local に native installer で入れる（root で実行すると /root に入る）。
# ~/.claude と ~/.config/gh は volume のマウント先なので、所有者が vscode の空ディレクトリをイメージに作っておく。
USER vscode
RUN curl -fsSL https://claude.ai/install.sh | bash \
    && mkdir -p /home/vscode/.claude /home/vscode/.config/gh
USER root
ENV PATH="/home/vscode/.local/bin:${PATH}"
