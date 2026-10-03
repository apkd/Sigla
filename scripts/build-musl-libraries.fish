#!/run/current-system/sw/bin/fish
# Build only the archive formats used by Sigla. CI caches the installed libraries.
if test (count $argv) -ne 1
    echo 'Usage: fish scripts/build-musl-libraries.fish INSTALL_DIRECTORY' >&2
    exit 2
end
set -l prefix (realpath -m $argv[1])
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
printf 'Bundled native libraries\n========================\n' >"$prefix/THIRD-PARTY-NOTICES.txt"

function notice --argument-names name --inherit-variable prefix
    printf '\n%s\n\n' $name >>"$prefix/THIRD-PARTY-NOTICES.txt"
    cat $argv[2..] >>"$prefix/THIRD-PARTY-NOTICES.txt"
    or exit 1
end

function unpack --argument-names name url checksum --inherit-variable work
    curl -fsSL --connect-timeout 15 --max-time 120 --retry 3 $url -o "$work/$name.tar"
    or exit 1
    printf '%s  %s\n' $checksum "$work/$name.tar" | sha256sum -c -
    or exit 1
    mkdir "$work/$name"
    and tar -xf "$work/$name.tar" -C "$work/$name" --strip-components=1
    and cd "$work/$name"
    or exit 1
end

unpack zlib https://zlib.net/fossils/zlib-1.3.2.tar.gz bb329a0a2cd0274d05519d61c667c062e06990d72e125ee2dfa8de64f0119d16
notice zlib LICENSE
./configure --static --prefix=$prefix
and make -j$jobs
and make install
or exit 1

# musl is supplied by Rust's target libraries; retain its license in the release.
curl -fsSL --connect-timeout 15 --max-time 120 --retry 3 \
    https://raw.githubusercontent.com/ifduyue/musl/v1.2.5/COPYRIGHT -o "$work/musl-COPYRIGHT"
or exit 1
printf '%s  %s\n' f9bc4423732350eb0b3f7ed7e91d530298476f8fec0c6c427a1c04ade22655af "$work/musl-COPYRIGHT" | sha256sum -c -
or exit 1
notice musl "$work/musl-COPYRIGHT"

unpack bzip2 https://sourceware.org/pub/bzip2/bzip2-1.0.8.tar.gz ab5a03176ee106d3f0fa90e381da478ddae405918153cca248e682cd0c4a2269
notice bzip2 LICENSE
make -j$jobs libbz2.a CC=$CC CFLAGS="$CFLAGS"
and install -m644 libbz2.a "$prefix/lib/"
and install -m644 bzlib.h "$prefix/include/"
or exit 1

unpack xz https://github.com/tukaani-project/xz/releases/download/v5.8.4/xz-5.8.4.tar.xz 4ce24038fd4221e0d13bc1a2de7a4db56e90b92b3bf75321f6c14be73f65de4b
notice liblzma COPYING COPYING.0BSD
./configure --host=x86_64-linux-musl --prefix=$prefix --libdir=$prefix/lib \
    --disable-shared --enable-static --disable-nls --disable-doc \
    --disable-xz --disable-xzdec --disable-lzmadec --disable-lzmainfo --disable-scripts
and make -j$jobs
and make install
or exit 1

unpack zstd https://github.com/facebook/zstd/releases/download/v1.5.7/zstd-1.5.7.tar.gz eb33e51f49a15e023950cd7825ca74a4a2b43db8354825ac24fc1b7ee09e6fa3
notice zstd LICENSE
make -C lib -j$jobs libzstd.a CC=$CC CFLAGS="$CFLAGS"
and install -m644 lib/libzstd.a "$prefix/lib/"
and install -m644 lib/zstd.h lib/zstd_errors.h "$prefix/include/"
or exit 1

unpack libarchive https://www.libarchive.org/downloads/libarchive-3.8.9.tar.xz 888c934f9d95648ecb9163dc8e23ab80a476ecb81a8f1154704a227b5b676dde
notice libarchive COPYING
./configure --host=x86_64-linux-musl --prefix=$prefix --libdir=$prefix/lib \
    --disable-shared --enable-static --disable-bsdtar --disable-bsdcat \
    --disable-bsdcpio --disable-bsdunzip --disable-acl --disable-xattr \
    --without-openssl --without-nettle --without-mbedtls --without-xml2 \
    --without-expat --without-libb2 --without-lz4 --without-lzo2
and make -j$jobs
and make install
or exit 1
