FROM python:3.12-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --uid 1000 rosetta

WORKDIR /app
COPY pyproject.toml README.md ./
COPY code_rosetta ./code_rosetta
RUN pip install --no-cache-dir . \
 && python -c "import sqlite3 as s; s.connect(':memory:').execute('create virtual table t using fts5(b, tokenize=\"trigram\")')"

USER 1000
# tree-sitter-language-pack fetches grammars on first use; bake them into the image.
RUN python -c "from code_rosetta.cli import _ensure_parsers; _ensure_parsers()" \
 && python -c "import tree_sitter_language_pack as t; print(t.downloaded_languages())"
ENV CODE_ROSETTA_CONFIG=/data/config.yaml \
    ROSETTA_REPOS_DIR=/data/repos \
    ROSETTA_DB=/data/rosetta.db \
    PYTHONUNBUFFERED=1
EXPOSE 8000
ENTRYPOINT ["code-rosetta"]
CMD ["serve", "--transport", "http", "--host", "0.0.0.0", "--port", "8000", "--read-only"]
