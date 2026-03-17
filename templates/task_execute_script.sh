export MAY_TASK_TRIGGER_FILE={{ trigger_file | safe }}
exec bash {{ script_file | safe }}
