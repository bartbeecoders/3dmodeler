Is it possible to use the box3d library to create a 3D modeler?

Architecture:
- rust 
- webassembly so we can host it in a web browser

Features:
- same UI, object interaction as Blender 3D
- basic shapes (cube, sphere, cylinder, etc.)
- view port navigation (orbit, pan, zoom)

Create a step by step plan for this project.
Save the plan in a plan.md file, so we can track our progress.
Put the code and plan in a folder called 3dmodeler



Improvements:

- the MMB drag - orbit, is inverse
- Alt-LMB does not work in the web browser, it gives a popup menu (from linux ?), this is actually also the case in blender

Basic Viewport Orbit (Quickest Way)Hold the Middle Mouse Button (MMB) and drag your mouse.This orbits/rotates the view around the current pivot point. 

docs.blender.org

Best Way: Focus + Orbit Around a Specific ObjectSelect the object (right-click or left-click depending on your select mode).
Press Numpad . (the period key on the number pad).  This frames/centers the view on the selected object and makes it the orbit center. 

reddit.com

Now hold Middle Mouse Button and drag to rotate smoothly around it.

This combo is the standard workflow.Make It Even Better (Recommended Setting)To automatically orbit around whatever you have selected (without always pressing .):Go to Edit > Preferences > Navigation.
Enable Orbit Around Selection. 

blenderartists.org

Now MMB drag will naturally rotate around you

Shift A does not work. I have an azerty keyboard, and it listens to shift+Q

Add a plan for following improvements, and start implementing:

- [ ] Grid system, with snap to grid
- [ ] metric units
- [ ] ability to add measurements
- [ ] object adornments (labels, dimensions, etc.)
- [ ] ability to duplicate objects, SHIFT+D like in blender
- [ ] ability to link objects (hierarchy)

Add drag and drop on the outliner to set the object hierarchy


MCP server
- [ ] implement mcp server so that coding agents can interact with the 3D modeler
- [ ] add a full documentation on how to setup the mcp server in claude code and other IDEs
- [ ] add an indication on the frontend that the MCP server is running


File management

Add the ability to save and load files to file storage. Use json format for the files.
Add a recent file list to the menu.
Use the extension .bee3d

Add key shortcuts:
ctrl-s = save
ctrl-o = open
ctrl-n = new
ctrl-z = undo
ctrl-y = redo

UI Improvements
- when moving objects, in a certain direction ((G + x,y,z)), show guide line
- when rotating objects, show rotation arcs

When moving opjects, allow for keyboard movements like:
G x 10 <enter> (move 10 units on the x-axis)
G y 5 <enter> (move 5 units on the y-axis)
R x 45 <enter> (rotate 45 degrees on the x-axis)

Push the code to github. I created a public repo: https://github.com/bartbeecoders/3dmodeler.git
Include all files from /run/media/bart/Development/Projects/box3d 


Add a settings page:

- maintain the grid size
- maintain the grid color
- ability to define a unit, standard is 1 unit = 1 meter
    - ability to switch between units (meter, centimeter, millimeter)
    - only allow metric
- ability to set the default save location

Make the settings page large enough to fit all settings and all future settings
Make it multi tabbed


Improvement:
moving, rotating, scaling objects should only be done on the selected object(s).
Not on the linked (children or parent) objects.


Object edit mode

Use the tab key to go in or out of object edit mode. 
In object edit mode, you can select objects and edit them.
Use & é " (1,2,3 on a azerty keyboard) to select the edit mode: face, edge, vertex
This will select a face, edge or vertex of the object.
The user can then edit the selected face, edge or vertex.
