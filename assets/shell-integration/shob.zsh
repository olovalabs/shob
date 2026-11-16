# Shob shell integration for zsh.
#
# Sourced after the user's .zshrc. Emits OSC 133 semantic prompt markers:
#   A = prompt start, B = prompt end / input start,
#   C = command accepted (output starts), D = command finished with exit code.
# Markers carry Shob's per-run token so TUIs that emit their own 133
# sequences are ignored.

[[ -o interactive ]] || return 0
[[ -n $SHOB_SHELL_INTEGRATION ]] && return 0
export SHOB_SHELL_INTEGRATION=1

# Keep the token in a shell variable and remove it from the environment so
# child processes (agents, TUIs) cannot stamp valid markers.
typeset -g __shob_token=$SHOB_SHELL_TOKEN
unset SHOB_SHELL_TOKEN

__shob_osc() {
  builtin printf '\e]%s\a' "$1"
}

__shob_precmd() {
  local ret=$?
  __shob_osc "133;D;${ret};SHOB=${__shob_token}"
  __shob_osc "7;file://${HOST}${PWD}"
  __shob_osc "7502;host=${HOST}"
  return $ret
}

__shob_preexec() {
  __shob_osc "133;C;SHOB=${__shob_token}"
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd __shob_precmd
add-zsh-hook preexec __shob_preexec

# A starts the prompt, B ends it. `%{...%}` keeps both zero-width.
typeset -g __shob_prompt_a=$'%{\e]133;A;SHOB='"${__shob_token}"$'\a%}'
typeset -g __shob_prompt_b=$'%{\e]133;B;SHOB='"${__shob_token}"$'\a%}'
PROMPT="${__shob_prompt_a}${PROMPT}${__shob_prompt_b}"
