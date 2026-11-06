# Both upstream fzf (with --zsh) and Ubuntu's older packaged integration.
if _tildy_fzf_init=$(fzf --zsh 2>/dev/null); then
  eval "$_tildy_fzf_init"
else
  _tildy_fzf_dir=/usr/share/doc/fzf/examples
  [[ -r $_tildy_fzf_dir/completion.zsh && -r $_tildy_fzf_dir/key-bindings.zsh ]] || return 1
  source "$_tildy_fzf_dir/completion.zsh" || return 1
  source "$_tildy_fzf_dir/key-bindings.zsh" || return 1
fi
unset _tildy_fzf_init _tildy_fzf_dir
(( $+widgets[fzf-history-widget] ))
