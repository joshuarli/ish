PATH_add() {
  for _denv_p in "$@"; do
    case "$_denv_p" in
      /*) ;;
      *) _denv_p="$PWD/$_denv_p" ;;
    esac
    PATH="$_denv_p${PATH:+:$PATH}"
    export PATH
  done
  unset _denv_p
}

path_add() {
  _denv_var=$1
  shift
  for _denv_p in "$@"; do
    case "$_denv_p" in
      /*) ;;
      *) _denv_p="$PWD/$_denv_p" ;;
    esac
    eval "_denv_old=\${$_denv_var-}"
    if [ -n "$_denv_old" ]; then
      eval "export $_denv_var=\$_denv_p:\$_denv_old"
    else
      eval "export $_denv_var=\$_denv_p"
    fi
  done
  unset _denv_var _denv_p _denv_old
}

PATH_rm() {
  for _denv_pattern in "$@"; do
    _denv_new_path=
    _denv_old_ifs=${IFS- }
    IFS=:
    for _denv_p in $PATH; do
      # shellcheck disable=SC2254
      case "$_denv_p" in
        $_denv_pattern) ;;
        *) _denv_new_path="${_denv_new_path:+$_denv_new_path:}$_denv_p" ;;
      esac
    done
    IFS=$_denv_old_ifs
    PATH=$_denv_new_path
    export PATH
  done
  unset _denv_pattern _denv_new_path _denv_old_ifs _denv_p
}

path_rm() {
  _denv_var=$1
  shift
  for _denv_pattern in "$@"; do
    eval "_denv_val=\${$_denv_var-}"
    _denv_new_path=
    _denv_old_ifs=${IFS- }
    IFS=:
    for _denv_p in $_denv_val; do
      # shellcheck disable=SC2254
      case "$_denv_p" in
        $_denv_pattern) ;;
        *) _denv_new_path="${_denv_new_path:+$_denv_new_path:}$_denv_p" ;;
      esac
    done
    IFS=$_denv_old_ifs
    eval "export $_denv_var=\$_denv_new_path"
  done
  unset _denv_var _denv_pattern _denv_val _denv_new_path _denv_old_ifs _denv_p
}

MANPATH_add() {
  for _denv_p in "$@"; do
    case "$_denv_p" in
      /*) ;;
      *) _denv_p="$PWD/$_denv_p" ;;
    esac
    if [ -n "${MANPATH-}" ]; then
      MANPATH="$_denv_p:$MANPATH"
    else
      MANPATH=$_denv_p
    fi
    export MANPATH
  done
  unset _denv_p
}

has() { command -v "$1" >/dev/null 2>&1; }
watch_file() { :; }
watch_dir() { :; }

expand_path() {
  case "$1" in
    ~/*) printf '%s\n' "$HOME/${1#~/}" ;;
    /*) printf '%s\n' "$1" ;;
    *) printf '%s\n' "$PWD/$1" ;;
  esac
}

find_up() {
  _denv_file=$1
  _denv_dir=$PWD
  while [ "$_denv_dir" != "/" ]; do
    if [ -e "$_denv_dir/$_denv_file" ]; then
      printf '%s\n' "$_denv_dir/$_denv_file"
      unset _denv_file _denv_dir
      return 0
    fi
    _denv_dir=${_denv_dir%/*}
    [ -z "$_denv_dir" ] && _denv_dir=/
  done
  unset _denv_file _denv_dir
  return 1
}

env_vars_required() {
  _denv_rc=0
  for _denv_var in "$@"; do
    eval "_denv_val=\${$_denv_var-}"
    if [ -z "$_denv_val" ]; then
      log_error "$_denv_var is required"
      _denv_rc=1
    fi
  done
  unset _denv_var _denv_val
  return $_denv_rc
}

load_prefix() {
  _denv_prefix=${1%/}
  case "$_denv_prefix" in
    /*) ;;
    *) _denv_prefix="$PWD/$_denv_prefix" ;;
  esac
  PATH_add "$_denv_prefix/bin" "$_denv_prefix/sbin"
  [ -d "$_denv_prefix/include" ] && path_add CPATH "$_denv_prefix/include"
  if [ -d "$_denv_prefix/lib" ]; then
    path_add PKG_CONFIG_PATH "$_denv_prefix/lib/pkgconfig"
    path_add LIBRARY_PATH "$_denv_prefix/lib"
    path_add DYLD_LIBRARY_PATH "$_denv_prefix/lib"
    path_add LD_LIBRARY_PATH "$_denv_prefix/lib"
  fi
  if [ -d "$_denv_prefix/lib64" ]; then
    path_add PKG_CONFIG_PATH "$_denv_prefix/lib64/pkgconfig"
    path_add LIBRARY_PATH "$_denv_prefix/lib64"
    path_add DYLD_LIBRARY_PATH "$_denv_prefix/lib64"
    path_add LD_LIBRARY_PATH "$_denv_prefix/lib64"
  fi
  [ -d "$_denv_prefix/share/man" ] && MANPATH_add "$_denv_prefix/share/man"
  unset _denv_prefix
  return 0
}

source_env() { # shellcheck disable=SC1090
  [ -f "$1" ] && . "$1"
}
source_env_if_exists() { # shellcheck disable=SC1090
  [ -f "$1" ] && . "$1" || :
}

source_up() {
  _denv_dir=$PWD
  while :; do
    _denv_dir=${_denv_dir%/*}
    [ -z "$_denv_dir" ] && _denv_dir=/
    if [ -f "$_denv_dir/.envrc" ]; then
      # shellcheck disable=SC1091
      . "$_denv_dir/.envrc"
      unset _denv_dir
      return 0
    fi
    [ "$_denv_dir" = "/" ] && break
  done
  unset _denv_dir
  return 1
}

source_up_if_exists() { source_up 2>/dev/null || :; }

dotenv() {
  _denv_file=${1:-.env}
  [ -f "$_denv_file" ] || return 1
  set -a
  # shellcheck disable=SC1090
  . "$_denv_file"
  set +a
  unset _denv_file
}

dotenv_if_exists() { dotenv "${1:-.env}" 2>/dev/null || :; }
log_status() { printf 'denv: %s\n' "$*" >&2; }
log_error() { printf 'denv: error: %s\n' "$*" >&2; }
strict_env() { set -euo pipefail; }
unstrict_env() { set +euo pipefail; }
