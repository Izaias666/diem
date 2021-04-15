module.exports = async ({ github, context, title, body, assignees, labels }) => {
  // find existing issue
  console.log(`Find existing issue with mathcing title`)
  let existing_issue
  const issues = await github.search.issuesAndPullRequests({
    q: `is:open is:issue repo:${ process.env.GITHUB_REPOSITORY } in:title ${title}`
  })
  existing_issue = issues.data.items.find(issue => issue.title === title)

  try {
    if (existing_issue) {
      // update in place
      const issue = await github.issues.update({
        issue_number: existing_issue.number,
        owner: context.repo.owner,
        repo: context.repo.repo,
        body: body
      })
      console.log(`Updated issue ${issue.data.number}`)
    } else {
      // create
      const issue  = await github.issues.create({
        owner: context.repo.owner,
        repo: context.repo.repo,
        assignees: assignees,
        labels: labels,
        title: title,
        body: body
      })
      console.log(`Created issue ${issue.data.number}`)
    }
  } catch (err) {
    console.error(`Failed to update or create issue\n`, err);
  }
}
