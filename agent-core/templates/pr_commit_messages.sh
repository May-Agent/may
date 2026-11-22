git log --reverse -z --format=%h%n%B {{ head }}{% for exclude in excludes %} ^{{ exclude }}{% endfor %}
