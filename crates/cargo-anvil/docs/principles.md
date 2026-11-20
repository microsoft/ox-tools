# Principles of cargo-anvil

* It is only a file generator - its responsibility is to generate files in a user's repo
* It needs to be installed only by users doing updates to the repo pipelines and checks, standard
  users don't need to have the tool installed to run checks the checks
* It maintains structural alignment between local checks and pipelines checks by making sure pipelines
  execute the same commands that are executed locally
* The generated files are the configuration that can be changed by its users - there is no
  additional anvil-specific configuration file
* It makes updates simple - if the user didn't make modifications, anvil updates the files as needed. If
  the user did make modifications, they only get pestered when the upstream files changed.