logout() {
  if [[ "$0" == -* ]]; then
    exit 0
  else
    raise -c 2 "this is not a login shell"
  fi
}
