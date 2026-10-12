{% if project_context or scoped_context %}
# Project Instructions

Each block is headed by its source path. Its rules apply in that directory. Deeper files override higher ones.

{% if project_context %}
{{ project_context }}
{% endif %}
{% if scoped_context %}
These files apply to subdirectories. Read the relevant file before you change files there:
{% for ctx in scoped_context %}- `{{ ctx.path }}`
{% endfor %}
{% endif %}
{% endif %}
