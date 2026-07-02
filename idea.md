ideally want to create a custom linter + coding "agent" (maybe a tool?) that allows for static parsing, building essentially a hierarchical graph that can highlight a lot of the key issues with code, providing detailed feedback for llm to fix (unless you it is easily done deterministically through own engine ie deleting a space or soemthing mundane). a lot of the key characterstics with ai coding are: exceesive explanations and comments throughout code, odd function names, not reusing functions, modules, using inefficient methods or not "clever"/computationally cheaper methods of accomplishing a task, but more than not, building things that are ad hoc to accomplish a specific task as opposed to operating around the infrastructure that already exists. 

i want to use rust because of its nuanced typechecking, efficiency, graph modules. we will start with a python-first checker, then generalize across other languages.

my idea: using a combination of AST (abstract syntax tree), distilling functions into algebraic effects (?), static analysis as well as an effects graph to highlight duplicitous functions, unused inputs/outputs, and inneficient code. (except used through a efficiency lens). this builds a good mental/ literal model of how a codebase operates while being extremely efficient. we can use this for twofold:
1) code efficiency, and anti "AI-SLOP". for this we can implement a variety of features similar to scanaislop, but extend further in a lot of clever ways. some things are just better implmented via simple rules 
2) infastructure cleanliness and good practices and nuanced code review and modularity. through this practice we can impelent things as good as a senior engineer, create and edit/fix code via this linter, and hopefully develop/workshop ai generated code that even bests a senior engineer.
3) repurposing the graph components for an AI agent or basic coding cli/engine which can power claude code or codex. this graph we built and workshop will allow us to reduce context needed to accomplish a task. ai typically works best in isolation or the finest code/inputs available. if we were able to essentially clear context for each individual task (figure out through lightweight engine or regex/intent) as opposed to related tasks where context is necessary, and provide the necessary harness information for an ai to generate code efficiently, and essentially generate its best work (ie solely function names that exsit in codebase, inputs etc) + this nuanced graph, ai will be able to operate more efficiently in a codebase with much better eye for nuance, effectivity, but also reduce context and token cost required to accomplish a task. perhaps a dumber model or more lightweight model can achieve the same end goal with this harness.

features:
- simple cli
- audit checker/daemon (was thinking we could create a little cute cli pet/tomagachi that runs everytime the checker is run and has a x/100% health score based on quality of code)
- graph component
- plug and play with vscode/cursor
- claude skill (last step)
- agent harness/feedback. through determinism, can enforce/check changes as an ai makes them when the engine tells ai to fix codebase
- comprehensive summary of everything that needs to be fixed
