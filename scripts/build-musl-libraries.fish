#!/run/current-system/sw/bin/fish
if test (count $argv) -ne 1
    echo 'Usage: fish scripts/build-musl-libraries.fish INSTALL_DIRECTORY' >&2
    exit 2
end
set -l prefix (realpath -m $argv[1])
set -l sources (path resolve (path dirname (status filename))/native-libraries.json)
set -l work (mktemp -d)
function cleanup --on-event fish_exit --inherit-variable work
    rm -r -- $work
end
set -gx CC (set -q CC; and echo $CC; or echo musl-gcc)
set -gx CFLAGS '-O3 -fPIC -ffunction-sections -fdata-sections'
set -gx CPPFLAGS "-I$prefix/include"
set -gx LDFLAGS "-L$prefix/lib"
set -gx PKG_CONFIG_LIBDIR "$prefix/lib/pkgconfig"
set -gx PKG_CONFIG_PATH ''
set -l jobs (nproc)
mkdir -p $prefix $work
or exit 1
function unpack --argument-names name --inherit-variable work --inherit-variable sources
    set -l url (jq -er --arg name $name '.[$name].url' $sources)
    or exit 1
    set -l checksum (jq -er --arg name $name '.[$name].sha256' $sources)
    or exit 1
    curl -fsSL --connect-timeout 15 --max-time 120 --retry 3 $url -o "$work/$name.tar"
    or exit 1
    printf '%s  %s\n' $checksum "$work/$name.tar" | sha256sum -c -
    or exit 1
    mkdir "$work/$name"
    and tar -xf "$work/$name.tar" -C "$work/$name" --strip-components=1
    and cd "$work/$name"
    or exit 1
end

unpack zlib
./configure --static --prefix=$prefix
and make -j$jobs
and make install
or exit 1

unpack bzip2
make -j$jobs libbz2.a CC=$CC CFLAGS="$CFLAGS"
and install -m644 libbz2.a "$prefix/lib/"
and install -m644 bzlib.h "$prefix/include/"
or exit 1

unpack xz
./configure --host=x86_64-linux-musl --prefix=$prefix --libdir=$prefix/lib \
    --disable-shared --enable-static --disable-nls --disable-doc \
    --disable-xz --disable-xzdec --disable-lzmadec --disable-lzmainfo --disable-scripts
and make -j$jobs
and make install
or exit 1

unpack zstd
make -C lib -j$jobs libzstd.a CC=$CC CFLAGS="$CFLAGS"
and install -m644 lib/libzstd.a "$prefix/lib/"
and install -m644 lib/zstd.h lib/zstd_errors.h "$prefix/include/"
or exit 1

unpack libarchive
./configure --host=x86_64-linux-musl --prefix=$prefix --libdir=$prefix/lib \
    --disable-shared --enable-static --disable-bsdtar --disable-bsdcat \
    --disable-bsdcpio --disable-bsdunzip --disable-acl --disable-xattr \
    --without-openssl --without-nettle --without-mbedtls --without-xml2 \
    --without-expat --without-libb2 --without-lz4 --without-lzo2
and make -j$jobs
and make install
or exit 1
