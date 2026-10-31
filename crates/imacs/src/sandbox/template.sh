set -eu
source_dir=$1
kind=$2
project=$3
jobs=$4
rust_package=${5:-}
stage=$(mktemp -d /tmp/imacs-template.XXXXXX)
trap 'rm -rf "$stage"' EXIT
# Copy tracked files, including local edits, without local credentials or host build output.
git -c safe.directory="$source_dir" -C "$source_dir" ls-files -z > "$stage/tracked"
while IFS= read -r -d '' file; do
    if [ -e "$source_dir/$file" ] || [ -L "$source_dir/$file" ]; then
        printf '%s\0' "$file"
    fi
done < "$stage/tracked" > "$stage/files"
mkdir "$stage/source"
tar -C "$source_dir" --null -T "$stage/files" -cf "$stage/source.tar"
tar -C "$stage/source" -xf "$stage/source.tar"
cd "$stage/source"
if [ "$kind" = rust ]; then
    if ! command -v rustup >/dev/null 2>&1; then
        curl --fail --location https://sh.rustup.rs -o "$stage/rustup.sh"
        sh "$stage/rustup.sh" -y --profile minimal
    fi
    export PATH="$HOME/.cargo/bin:$PATH"
    rustup toolchain install stable --profile minimal --component rustfmt,clippy
    rustup default stable
    mkdir -p "$HOME/.cargo" "$HOME/.cache/imacs/target"
    printf '[build]\ntarget-dir = "%s/.cache/imacs/target"\njobs = %s\n' "$HOME" "$jobs" > "$HOME/.cargo/config.toml"
    if [ -n "$rust_package" ]; then
        cargo check -p "$rust_package"
    else
        cargo check --workspace
    fi
else
    mkdir -p "$HOME/.dotnet"
    curl --fail --location https://dot.net/v1/dotnet-install.sh -o "$stage/dotnet-install.sh"
    # Use global.json when present, otherwise install the fallback SDK.
    if [ -f global.json ]; then
        bash "$stage/dotnet-install.sh" --jsonfile global.json --install-dir "$HOME/.dotnet"
    else
        bash "$stage/dotnet-install.sh" --channel 8.0 --install-dir "$HOME/.dotnet"
    fi
    # Agent shells find the SDK as well as this preparation command.
    sudo ln -sf "$HOME/.dotnet/dotnet" /usr/local/bin/dotnet
    export DOTNET_ROOT="$HOME/.dotnet"
    export DOTNET_PROCESSOR_COUNT="$jobs" MSBUILDDISABLENODEREUSE=1 DOTNET_CLI_USE_MSBUILD_SERVER=0
    dotnet build "$project" --disable-build-servers -m:1 -p:UseSharedCompilation=false
fi
