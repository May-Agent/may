Import my {{ product_name }} global rules into May.

Read {{ rules_file }}. It becomes ~/.may/AGENTS.md, the global rules loaded into every May session. Compare it with your own system prompt: a rule that conflicts with it, repeats it, or relies on something May does not have needs a change.

- If no rule needs a change, run `global-rules keep`, which installs the file unchanged.
- Otherwise ask me through `ask_user` about each change, showing the rule and the change you propose, and end your turn. Once I have answered, write the file with only the changes I approved to your session directory and run `global-rules write <file>`.
