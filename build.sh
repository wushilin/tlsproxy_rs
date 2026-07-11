#!/bin/sh

#!/bin/sh
set -eu

echo "Checking embedded UI"
test -f static/index.html
test -f static/app.js
test -f static/styles.css
if grep -REn 'https?://' static/index.html static/app.js static/styles.css; then
  echo "External runtime URL found in embedded UI" >&2
  exit 1
fi

echo "Building binary"
cargo build --locked --release
echo "Done"
