# Kathamo shell integration for zsh.
#
# Sourced after the user's .zshrc. Emits OSC 133 semantic prompt markers:
#   A = prompt start, B = prompt end / input start,
#   C = command accepted (output starts), D = command finished with exit code.
# Markers carry Kathamo's per-run token so TUIs that emit their own 133
# sequences are ignored.

[[ -o interactive ]] || return 0
[[ -n $KATHAMO_SHELL_INTEGRATION ]] && return 0
export KATHAMO_SHELL_INTEGRATION=1

# Keep the token in a shell variable and remove it from the environment so
# child processes (agents, TUIs) cannot stamp valid markers.
typeset -g __kathamo_token=$KATHAMO_SHELL_TOKEN
unset KATHAMO_SHELL_TOKEN

__kathamo_osc() {
  builtin printf '\e]%s\a' "$1"
}

__kathamo_precmd() {
  local ret=$?
  __kathamo_osc "133;D;${ret};KATHAMO=${__kathamo_token}"
  __kathamo_osc "7;file://${HOST}${PWD}"
  __kathamo_osc "7502;host=${HOST}"
  return $ret
}

__kathamo_preexec() {
  __kathamo_osc "133;C;KATHAMO=${__kathamo_token}"
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd __kathamo_precmd
add-zsh-hook preexec __kathamo_preexec

# A starts the prompt, B ends it. `%{...%}` keeps both zero-width.
typeset -g __kathamo_prompt_a=$'%{\e]133;A;KATHAMO='"${__kathamo_token}"$'\a%}'
typeset -g __kathamo_prompt_b=$'%{\e]133;B;KATHAMO='"${__kathamo_token}"$'\a%}'
PROMPT="${__kathamo_prompt_a}${PROMPT}${__kathamo_prompt_b}"
