alias smoke.echo {
  echo -a $1-
}

on 1:TEXT:*smoke*:#:{
  smoke.echo $1-
}
