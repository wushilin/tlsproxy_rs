#!/bin/sh
#
ng build --configuration production
rm ../static/*
cp dist/angular-ui/* ../static/
