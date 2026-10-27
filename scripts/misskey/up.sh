#!/usr/bin/env bash
# 開発用の使い捨て Misskey を立て、bot 用のアカウントとカスタム絵文字を作って接続情報を .env.local に書く。
# 2 回目以降は、.env.local があればサーバを立てるだけにする。
# docker・curl・jq が要る。
set -euo pipefail
cd "$(dirname "$0")"

compose=(docker compose -p misskey-emoji-notify-misskey -f compose.yml)
base=http://localhost:3000

# api <token|""> <endpoint> [json]: 応答のボディを出力する。エラー応答なら失敗する。
api() {
    local token=$1 endpoint=$2 body=${3:-'{}'}
    if [ -n "$token" ]; then body=$(jq -c --arg i "$token" '. + {i: $i}' <<<"$body"); fi
    curl --fail-with-body -sS -m 60 -H 'Content-Type: application/json' -d "$body" "$base/api/$endpoint"
}

# upload <token> <path>: ドライブにファイルを上げ、その ID を出力する。
upload() {
    local token=$1 path=$2
    curl --fail-with-body -sS -m 60 -F "i=$token" -F "file=@$path" "$base/api/drive/files/create" | jq -r .id
}

"${compose[@]}" up -d --quiet-pull
echo "Misskey の起動を待っている" >&2
for _ in $(seq 150); do
    if api "" meta > /dev/null 2>&1; then break; fi
    sleep 2
done
api "" meta > /dev/null

if [ -f .env.local ]; then
    echo "既存の .env.local を使う" >&2
else
    # 最初のアカウントは認証なしで作れて、管理者になる。bot のアカウントは管理者が作る。
    admin=$(api "" admin/accounts/create '{"username":"admin","password":"password"}' | jq -r .token)
    # 続けて叩いてもレートリミットに掛からないよう、既定ロールの係数を下げる
    api "$admin" admin/roles/update-default-policies '{"policies":{"rateLimitFactor":0.3}}' > /dev/null
    bot=$(api "$admin" admin/accounts/create '{"username":"bot","password":"password"}' | jq -r .token)
    # bot が確かめるカスタム絵文字。画像はテストのフィクスチャを使う
    red=$(upload "$admin" ../../tests/fixtures/images/red.png)
    blue=$(upload "$admin" ../../tests/fixtures/images/blue.png)
    api "$admin" admin/emoji/add "$(jq -nc --arg f "$red" '{name:"red_square",fileId:$f,category:"shapes",aliases:["red"]}')" > /dev/null
    api "$admin" admin/emoji/add "$(jq -nc --arg f "$blue" '{name:"blue_bar",fileId:$f,category:"shapes",aliases:[]}')" > /dev/null
    api "$admin" admin/emoji/add "$(jq -nc --arg f "$red" '{name:"uncategorized",fileId:$f,aliases:["misc"]}')" > /dev/null
    cat > .env.local <<ENV
MISSKEY_SERVER=$base
MISSKEY_TOKEN=$bot
# 管理者のトークン。絵文字の追加・変更・削除（admin/emoji/*）で bot の通知を確かめるのに使う（ユーザー名 admin、パスワード password）
MISSKEY_ADMIN_TOKEN=$admin
ENV
    echo "アカウントと絵文字を作り、接続情報を .env.local に書いた" >&2
fi

cat <<USAGE
Misskey 2026.3.2 が $base で動いている。
bot を動かすには、scripts/misskey/.env.local の MISSKEY_SERVER と MISSKEY_TOKEN を config.toml の [misskey] に写してから、次を実行する。
  cargo run
Misskey の往復のテストは、接続情報を環境に読み込んでから実行する。
  set -a; . scripts/misskey/.env.local; set +a
  cargo test -- --ignored live_round_trip
止めるときは scripts/misskey/down.sh（データも消える）。
USAGE
