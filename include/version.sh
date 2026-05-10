version() {
  local verbose=0
  while getopts 'v' opt; do
    case $opt in
      v) verbose=1 ;;
    esac
  done

  local maj="${SHED_VER_INFO[major]}"
  local min="${SHED_VER_INFO[minor]}"
  local patch="${SHED_VER_INFO[patch]}"
  local arch="${SHED_VER_INFO[arch]}"
  local os="${SHED_VER_INFO[os]}"

  local semver="${maj}.${min}.${patch}"
  local triple="shed v${semver} (${arch} ${os})"

  if [[ $verbose -gt 0 ]]; then
    echo "$triple"
  else
    echo "$semver"
  fi
}
