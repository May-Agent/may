validate_path={{ validate_path | safe }}
printf '%s' {{ encoded_script | safe }} | base64 --decode > "$validate_path"
bash -n "$validate_path"
exit_code=$?
rm -f "$validate_path"
exit $exit_code
