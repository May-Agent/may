export MAY_TASK_TRIGGER_FILE={{ trigger_file | safe }}
export MAY_TASK_TRIGGER_KIND={{ trigger_kind | safe }}
exec bash {{ script_file | safe }}
