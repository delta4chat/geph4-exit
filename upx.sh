#!/bin/bash

# a warpper for "post-run" build scripts
# see also https://github.com/rust-lang/cargo/issues/545

tmp=$(mktemp || exit)
trap "rm $tmp" EXIT

tee $tmp <<"EOF"
if type upx
then
	upx $1 && exit
fi

if test "$(cat $1|wc -c)" -lt 31457280
then
	echo skipping file size less than 30M
	exit
fi

if type xz
then
	cat $1 | xz -c -v -e -9 > $1.xz && mv $1.xz $1 && exit
fi

if type gzip
then
	cat $1 | gzip -c -9 > $1.gz && mv $1.gz $1 && exit
fi

echo failed to compress file size, fallback to strip.

strip $1
EOF

command $*
status_code="$?"
find ./target/ \( -name geph4-exit -or -name geph4-exit.exe \) -exec sh $tmp '{}' \;
exit "$status_code"

